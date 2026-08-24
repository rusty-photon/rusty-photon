#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
//! Sentinel - Observatory monitoring and notification service
//!
//! Polls ASCOM Alpaca devices, detects state transitions, and sends notifications.

// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        clippy::used_underscore_binding,
        clippy::significant_drop_tightening,
        clippy::significant_drop_in_scrutinee,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::suboptimal_flops,
        clippy::too_many_lines,
        clippy::option_if_let_else,
        clippy::match_same_arms,
        clippy::float_cmp,
        clippy::similar_names,
        clippy::struct_excessive_bools,
    )
)]

pub mod alpaca_client;
pub mod config;
pub mod corrective;
pub mod dashboard;
pub mod discovery;
pub mod doctor;
pub mod engine;
pub mod error;
pub mod health;
pub mod io;
pub mod monitor;
pub mod notifier;
pub mod pushover;
pub mod restart;
pub mod state;
pub mod watchdog;

pub use config::{load_config, Config};
pub use error::{Result, SentinelError};

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::alpaca_client::AlpacaSafetyMonitor;
use crate::corrective::{Corrective, CorrectiveLadder};
use crate::discovery::{ServiceManager, ServiceRegistry, SupervisionPolicy};
use crate::engine::Engine;
use crate::health::{DiscoverySupervisor, SupervisionContext};
use crate::io::ReqwestHttpClient;
use crate::monitor::Monitor;
use crate::notifier::Notifier;
use crate::pushover::PushoverNotifier;
use crate::state::StateHandle;
use crate::watchdog::{
    EventMonitor, HttpWatchdogEventSource, OperationDeadlineMonitor, WatchdogEventSource,
};

/// Factory methods for building monitors and notifiers from config.
///
/// These live in `lib.rs` (rather than `config.rs`) because they depend on
/// concrete types (`AlpacaSafetyMonitor`, `PushoverNotifier`) that are defined
/// in sibling modules.
impl Config {
    pub fn build_monitors(
        &self,
        http: &Arc<dyn io::HttpClient>,
        ca_path: Option<&std::path::Path>,
    ) -> Vec<Arc<dyn Monitor>> {
        self.monitors
            .iter()
            .map(|monitor_config| -> Arc<dyn Monitor> {
                match monitor_config {
                    config::MonitorConfig::AlpacaSafetyMonitor { auth, .. } => {
                        let client: Arc<dyn io::HttpClient> = auth.as_ref().map_or_else(
                            || Arc::clone(http),
                            |a| match ReqwestHttpClient::with_auth(
                                ca_path,
                                a.username.clone(),
                                a.password.clone(),
                            ) {
                                Ok(c) => Arc::new(c),
                                Err(e) => {
                                    tracing::error!(
                                        "Failed to build auth HTTP client: {e}. \
                                         Falling back to shared client."
                                    );
                                    Arc::clone(http)
                                }
                            },
                        );
                        Arc::new(AlpacaSafetyMonitor::new(monitor_config, client))
                    }
                }
            })
            .collect()
    }

    pub fn build_notifiers(&self, http: &Arc<dyn io::HttpClient>) -> Vec<Arc<dyn Notifier>> {
        self.notifiers
            .iter()
            .map(|notifier_config| -> Arc<dyn Notifier> {
                match notifier_config {
                    config::NotifierConfig::Pushover { .. } => {
                        Arc::new(PushoverNotifier::new(notifier_config, Arc::clone(http)))
                    }
                }
            })
            .collect()
    }

    /// Build the push-based event monitors from config. Today this is the
    /// optional `operation_watchdog`; an absent block yields no event
    /// monitors (safety-polling-only behavior). The watchdog's corrective
    /// ladder shares the same `http` client used for polling, the same
    /// restart `gate` as every other restart path, and resolves
    /// `operations.<family>.service` against the discovered-services
    /// `registry`.
    #[allow(clippy::too_many_arguments)]
    pub fn build_event_monitors(
        &self,
        notifiers: &[Arc<dyn Notifier>],
        state: &StateHandle,
        http: &Arc<dyn io::HttpClient>,
        gate: restart::RestartGate,
        registry: &ServiceRegistry,
        manager: &Arc<dyn ServiceManager>,
        restart_budget: std::time::Duration,
    ) -> Vec<Arc<dyn EventMonitor>> {
        self.operation_watchdog
            .as_ref()
            .map_or_else(Vec::new, |watchdog| {
                let source: Arc<dyn WatchdogEventSource> =
                    Arc::new(HttpWatchdogEventSource::new(&watchdog.rp_url));
                let corrective: Arc<dyn Corrective> = Arc::new(CorrectiveLadder::http(
                    Arc::clone(http),
                    gate,
                    Arc::clone(manager),
                    restart_budget,
                ));
                let monitor: Arc<dyn EventMonitor> = Arc::new(OperationDeadlineMonitor::new(
                    "Operation Watchdog",
                    source,
                    notifiers.to_vec(),
                    Arc::clone(state),
                    watchdog.clone(),
                    Arc::clone(registry),
                    corrective,
                ));
                vec![monitor]
            })
    }
}

/// Builder for the sentinel service with injectable dependencies
pub struct SentinelBuilder {
    config: Config,
    http: Arc<dyn io::HttpClient>,
    cancel: CancellationToken,
    monitors: Option<Vec<Arc<dyn Monitor>>>,
    event_monitors: Option<Vec<Arc<dyn EventMonitor>>>,
    notifiers: Option<Vec<Arc<dyn Notifier>>>,
    service_manager: Option<Arc<dyn ServiceManager>>,
    config_dir: Option<std::path::PathBuf>,
}

impl SentinelBuilder {
    /// Create a new builder with production defaults.
    ///
    /// When `config.ca_cert` is set, the HTTP client trusts that CA for
    /// connecting to TLS-enabled Alpaca services.
    pub fn new(config: Config) -> Self {
        let ca_path = config
            .ca_cert
            .as_deref()
            .map(rusty_photon_tls::config::expand_tilde);
        let http: Arc<dyn io::HttpClient> = match ReqwestHttpClient::new(ca_path.as_deref()) {
            Ok(client) => Arc::new(client),
            Err(e) => {
                tracing::error!("Failed to build HTTP client with CA cert: {e}. Falling back to default client.");
                Arc::new(ReqwestHttpClient::default())
            }
        };
        Self {
            config,
            http,
            cancel: CancellationToken::new(),
            monitors: None,
            event_monitors: None,
            notifiers: None,
            service_manager: None,
            config_dir: None,
        }
    }

    /// Override the HTTP client (useful for testing)
    #[must_use]
    pub fn with_http_client(mut self, http: Arc<dyn io::HttpClient>) -> Self {
        self.http = http;
        self
    }

    /// Override the cancellation token (useful for testing)
    #[must_use]
    pub fn with_cancellation_token(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Inject pre-built monitors instead of constructing them from config
    #[must_use]
    pub fn with_monitors(mut self, monitors: Vec<Arc<dyn Monitor>>) -> Self {
        self.monitors = Some(monitors);
        self
    }

    /// Inject pre-built event monitors (e.g. a watchdog over a mock event
    /// source) instead of constructing them from config
    #[must_use]
    pub fn with_event_monitors(mut self, event_monitors: Vec<Arc<dyn EventMonitor>>) -> Self {
        self.event_monitors = Some(event_monitors);
        self
    }

    /// Inject pre-built notifiers instead of constructing them from config
    #[must_use]
    pub fn with_notifiers(mut self, notifiers: Vec<Arc<dyn Notifier>>) -> Self {
        self.notifiers = Some(notifiers);
        self
    }

    /// Override the platform service manager (useful for testing). Without
    /// this, the manager comes from
    /// [`discovery::service_manager_from_env`] — the platform backend, or
    /// the directory-backed stub when `SENTINEL_SERVICE_MANAGER_DIR` is set.
    #[must_use]
    pub fn with_service_manager(mut self, manager: Arc<dyn ServiceManager>) -> Self {
        self.service_manager = Some(manager);
        self
    }

    /// The directory sentinel's own config file lives in — where discovery
    /// reads the supervised services' `<svc>.json` siblings to derive their
    /// probe URLs. Without it, health reports `unknown` for every service.
    #[must_use]
    pub fn with_config_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.config_dir = Some(dir);
        self
    }

    /// Build the sentinel service: constructs monitors, notifiers, engine, connects monitors,
    /// and binds the dashboard listener if enabled.
    ///
    /// # Errors
    ///
    /// Never errors in the current implementation — a dashboard bind
    /// failure degrades to running without the dashboard (logged), and
    /// every other step is infallible. The `Result` keeps the fleet's
    /// builder contract.
    pub async fn build(self) -> Result<Sentinel> {
        let http = self.http;
        let cancel = self.cancel;
        let config = self.config;

        // Use injected monitors/notifiers or fall back to config factories
        let ca_path = config
            .ca_cert
            .as_deref()
            .map(rusty_photon_tls::config::expand_tilde);
        let monitors = self
            .monitors
            .unwrap_or_else(|| config.build_monitors(&http, ca_path.as_deref()));
        let notifiers = self
            .notifiers
            .unwrap_or_else(|| config.build_notifiers(&http));

        // Build shared state. Service snapshots are populated by discovery,
        // not seeded from config.
        let monitors_with_intervals: Vec<(String, std::time::Duration)> = monitors
            .iter()
            .map(|m| (m.name().to_string(), m.polling_interval()))
            .collect();
        let state = state::new_state_handle(monitors_with_intervals, config.dashboard.history_size);

        // Service discovery: the platform service manager (or the test
        // stub), the supervision policy constants, and the shared registry
        // every consumer resolves against.
        let manager = self
            .service_manager
            .unwrap_or_else(discovery::service_manager_from_env);
        let policy = SupervisionPolicy::resolve_from_env();
        let registry: ServiceRegistry =
            Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));

        // The Service Restart API's engine, over the discovered-services
        // registry. Built before the event monitors so its restart gate can
        // be shared with the watchdog ladder and the health supervisors.
        let restarts = Arc::new(restart::RestartManager::new(
            Arc::clone(&registry),
            Arc::clone(&manager),
            policy.restart_budget,
        ));

        // Build event monitors (the operation watchdog) — they escalate
        // through the notifier chain and record into shared state.
        let mut event_monitors = self.event_monitors.unwrap_or_else(|| {
            config.build_event_monitors(
                &notifiers,
                &state,
                &http,
                restarts.gate(),
                &registry,
                &manager,
                policy.restart_budget,
            )
        });

        // The discovery loop (registry upkeep + universal health
        // supervision) is appended after the DI override so injecting custom
        // event monitors never silently disables supervision. The probe
        // client's trust and auth posture — pinned CA, platform roots, or
        // unauthenticated with verification off — is decided by
        // `build_probe_client`; see its doc for the three states.
        let probe_http =
            build_probe_client(config.service_auth.as_ref(), ca_path.as_deref(), &http);
        let supervision = DiscoverySupervisor::new(
            Arc::clone(&manager),
            self.config_dir.clone(),
            config.probe_domain.as_ref().map(|d| d.as_str().to_string()),
            SupervisionContext {
                policy,
                registry: Arc::clone(&registry),
                http: probe_http,
                restarts: Arc::clone(&restarts),
                notifiers: notifiers.clone(),
                state: Arc::clone(&state),
            },
        );
        // Run the first discovery pass before the dashboard binds, so the
        // restart endpoint and /api/services never race an empty registry at
        // startup.
        supervision.refresh().await;
        event_monitors.push(Arc::new(supervision));

        // Build engine
        let engine = Engine::new(
            monitors,
            event_monitors,
            notifiers,
            config.transitions.clone(),
            Arc::clone(&state),
            cancel.clone(),
        );

        // Connect monitors
        engine.connect_all().await;

        // Bind dashboard listener if enabled
        let dashboard_listener = if config.dashboard.enabled {
            let addr = config.server.socket_addr();
            match tokio::net::TcpListener::bind(addr).await {
                Ok(listener) => {
                    tracing::debug!("Dashboard bound to {}", addr);
                    Some(listener)
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to bind dashboard to {}: {}. Continuing without dashboard.",
                        addr,
                        e
                    );
                    None
                }
            }
        } else {
            None
        };

        let dashboard_tls = config.server.tls.clone();
        let dashboard_auth = config.server.auth.clone();

        Ok(Sentinel {
            engine,
            state,
            cancel,
            dashboard_listener,
            dashboard_tls,
            dashboard_auth,
            restarts,
        })
    }
}

/// A fully constructed sentinel service ready to run
pub struct Sentinel {
    engine: Engine,
    state: state::StateHandle,
    cancel: CancellationToken,
    dashboard_listener: Option<tokio::net::TcpListener>,
    dashboard_tls: Option<rusty_photon_tls::config::TlsConfig>,
    dashboard_auth: Option<rp_auth::config::AuthConfig>,
    restarts: Arc<restart::RestartManager>,
}

impl Sentinel {
    /// Returns whether the dashboard listener was successfully bound during build.
    pub const fn has_dashboard(&self) -> bool {
        self.dashboard_listener.is_some()
    }

    /// Start the sentinel service: runs the polling loop until cancelled, then disconnects.
    ///
    /// Shutdown is propagated through the [`CancellationToken`] supplied
    /// to [`SentinelBuilder::with_cancellation_token`]. In production the
    /// binary builds that token from `rusty_photon_service_lifecycle::
    /// Shutdown::token()`, so the runner's OS-signal watcher drives this
    /// shutdown; in tests it is driven directly.
    ///
    /// # Errors
    ///
    /// Returns [`SentinelError::Io`] if reading the bound dashboard
    /// listener's address back fails. The dashboard serve loop runs
    /// detached and its failures are logged, never returned.
    pub async fn start(self) -> Result<()> {
        let cancel = self.cancel;

        // Start dashboard if we have a bound listener
        if let Some(listener) = self.dashboard_listener {
            let dashboard_state = Arc::clone(&self.state);
            let cancel_for_dashboard = cancel.clone();
            let dashboard_tls = self.dashboard_tls;
            let dashboard_auth = self.dashboard_auth;
            let restarts = Arc::clone(&self.restarts);

            let addr = listener.local_addr()?;
            let scheme = if dashboard_tls.is_some() {
                "https"
            } else {
                "http"
            };
            tracing::info!("Dashboard listening on {scheme}://{}", addr);
            // Console mode only: stdout is a dead handle under the Windows SCM,
            // and the only stdout consumer (bdd-infra's port parser) never runs
            // services with --service.
            if !rusty_photon_service_lifecycle::is_scm_service() {
                println!("Sentinel dashboard bound_addr={addr}");
            }

            tokio::spawn(async move {
                let router = dashboard::build_router(dashboard_state, restarts);

                // Layer authentication if configured
                let router = match &dashboard_auth {
                    Some(auth) => {
                        if dashboard_tls.is_none() {
                            tracing::warn!(
                                "Authentication is enabled but TLS is not. \
                                 Credentials will be transmitted in cleartext. \
                                 Consider enabling TLS (see `doctor --fix`)."
                            );
                        }
                        rp_auth::layer(router, auth)
                    }
                    None => router,
                };

                match dashboard_tls {
                    Some(ref tls_config) => {
                        rusty_photon_tls::server::serve_tls(
                            listener,
                            router,
                            tls_config,
                            async move {
                                cancel_for_dashboard.cancelled().await;
                            },
                        )
                        .await
                        .ok();
                    }
                    None => {
                        axum::serve(listener, router)
                            .with_graceful_shutdown(async move {
                                cancel_for_dashboard.cancelled().await;
                            })
                            .await
                            .ok();
                    }
                }

                tracing::debug!("Dashboard stopped");
            });
        }

        tracing::info!("Sentinel engine started");

        // Run the engine (blocks until cancelled)
        self.engine.run().await;

        // Disconnect monitors
        self.engine.disconnect_all().await;
        tracing::info!("Sentinel engine stopped");

        Ok(())
    }
}

/// The client the health-supervision probes use, keyed off the
/// doctor-written `service_auth` credential and the optional `ca_cert` pin
/// (a pin *replaces* the platform trust roots — ADR-002):
///
/// - credential + pin: authenticated probes (HTTP Basic, https requests
///   only — see [`ReqwestHttpClient`]) verifying peers against the pinned
///   observatory CA — the self-signed install doctor provisions.
/// - credential, no pin: authenticated probes verifying against the
///   **platform trust roots** — the ACME install, where the pin must be
///   absent (it would reject the publicly-trusted peers) and the platform
///   store is exactly what verifies them. An absent pin is a trust choice,
///   never a loss of capability.
/// - no credential: unauthenticated probes with certificate verification
///   off — a probe sends no credentials and never parses the body, and
///   sentinel cannot assume it holds a CA for every peer's self-signed
///   certificate; auth-on peers answer 401, which is still proof of life.
///
/// A credential whose pinned client cannot be built (`ca_cert` names a
/// missing or unreadable file) is the one misconfiguration: it degrades
/// loudly to the unauthenticated verification-off client, so the
/// credential never rides an unverified connection and the self-signed
/// peers the pin was for keep probing as alive rather than down. If even
/// that client cannot be built, fall back to the `shared` (verifying)
/// client loudly — self-signed TLS peers would then probe as down.
/// The remediation to append when the authenticated probe client cannot
/// be built — only a pinned install has a `ca_cert` to fix or remove.
const fn degraded_probe_advice(has_pin: bool) -> &'static str {
    if has_pin {
        " Fix or remove ca_cert to restore authenticated probes."
    } else {
        ""
    }
}

fn build_probe_client(
    service_auth: Option<&rp_auth::config::ClientAuthConfig>,
    ca_path: Option<&std::path::Path>,
    shared: &Arc<dyn io::HttpClient>,
) -> Arc<dyn io::HttpClient> {
    let insecure_probe_client = || -> Arc<dyn io::HttpClient> {
        match ReqwestHttpClient::insecure() {
            Ok(client) => Arc::new(client),
            Err(e) => {
                tracing::error!(
                    "{e}; probing through the shared verifying client — \
                     self-signed TLS peers may report down"
                );
                Arc::clone(shared)
            }
        }
    };
    match (service_auth, ca_path) {
        (Some(auth), ca) => {
            match ReqwestHttpClient::with_auth(ca, auth.username.clone(), auth.password.clone()) {
                Ok(client) => {
                    if ca.is_none() {
                        tracing::debug!(
                            "service_auth is set with no ca_cert pin — probing \
                         authenticated, verifying peers against the platform \
                         trust roots"
                        );
                    }
                    Arc::new(client)
                }
                Err(e) => {
                    let advice = degraded_probe_advice(ca.is_some());
                    tracing::error!(
                        "failed to build the authenticated probe client: {e}; \
                     probing unauthenticated with certificate verification \
                     off so the credential never rides an unverified \
                     connection — auth-on peers answer 401 (still proof of \
                     life).{advice}"
                    );
                    insecure_probe_client()
                }
            }
        }
        (None, _) => insecure_probe_client(),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::MonitorConfig;
    use crate::io::MockHttpClient;

    fn observatory_auth() -> rp_auth::config::ClientAuthConfig {
        rp_auth::config::ClientAuthConfig {
            username: "observatory".to_string(),
            password: "secret".to_string(),
        }
    }

    /// A TLS peer presenting a certificate from a throwaway CA, echoing
    /// whether the request carried an `Authorization` header. Returns the
    /// CA path, the probe URL, and a guard whose drop stops the server.
    async fn spawn_tls_peer(
        dir: &std::path::Path,
    ) -> (std::path::PathBuf, String, tokio::sync::oneshot::Sender<()>) {
        rusty_photon_tls::test_cert::generate_ca(dir).unwrap();
        let ca_pem = std::fs::read_to_string(dir.join("ca.pem")).unwrap();
        let ca_key_pem = std::fs::read_to_string(dir.join("ca-key.pem")).unwrap();
        rusty_photon_tls::test_cert::generate_service_cert(&ca_pem, &ca_key_pem, "peer", dir)
            .unwrap();
        let tls = rusty_photon_tls::config::TlsConfig {
            cert: dir.join("peer.pem").to_string_lossy().into_owned(),
            key: dir.join("peer-key.pem").to_string_lossy().into_owned(),
        };
        let router = axum::Router::new().route(
            "/health",
            axum::routing::get(|headers: axum::http::HeaderMap| async move {
                if headers.contains_key(axum::http::header::AUTHORIZATION) {
                    "authenticated"
                } else {
                    "anonymous"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            rusty_photon_tls::server::serve_tls(listener, router, &tls, async move {
                stopped.await.ok();
            })
            .await
            .ok();
        });
        (
            dir.join("ca.pem"),
            format!("https://localhost:{port}/health"),
            stop,
        )
    }

    #[tokio::test]
    async fn probe_client_with_credential_and_pin_authenticates_against_pinned_ca() {
        let dir = tempfile::tempdir().unwrap();
        let (ca, url, _stop) = spawn_tls_peer(dir.path()).await;
        let shared: Arc<dyn io::HttpClient> = Arc::new(MockHttpClient::new());
        let client = build_probe_client(Some(&observatory_auth()), Some(&ca), &shared);
        let response = client.get(&url).await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "authenticated");
    }

    #[tokio::test]
    async fn probe_client_with_credential_and_no_pin_verifies_against_platform_roots() {
        let dir = tempfile::tempdir().unwrap();
        let (_ca, url, _stop) = spawn_tls_peer(dir.path()).await;
        let shared: Arc<dyn io::HttpClient> = Arc::new(MockHttpClient::new());
        // Preflight through the verification-off client: the peer is up
        // and answering, so the failure asserted below can only be the
        // handshake — not a dead listener.
        let insecure = build_probe_client(None, None, &shared);
        assert_eq!(insecure.get(&url).await.unwrap().status, 200);
        let client = build_probe_client(Some(&observatory_auth()), None, &shared);
        // The peer's throwaway CA is not in the platform trust store, so
        // the handshake must be refused — verification stays on with no
        // pin (issue #810: absent means platform roots, not verification
        // off).
        let err = client.get(&url).await.unwrap_err();
        match &err {
            crate::SentinelError::Http(msg) => {
                assert!(msg.starts_with(&format!("GET {url} failed:")), "{msg}");
                assert!(
                    msg.contains("certificate"),
                    "expected a certificate verification error, got: {msg}"
                );
            }
            other => panic!("expected SentinelError::Http, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn probe_client_with_unusable_pin_withholds_credential_and_skips_verification() {
        let dir = tempfile::tempdir().unwrap();
        let (_ca, url, _stop) = spawn_tls_peer(dir.path()).await;
        let missing = dir.path().join("no-such-ca.pem");
        let shared: Arc<dyn io::HttpClient> = Arc::new(MockHttpClient::new());
        let client = build_probe_client(Some(&observatory_auth()), Some(&missing), &shared);
        // The pinned client cannot be built, so probing degrades to the
        // verification-off client — which never carries the credential.
        let response = client.get(&url).await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "anonymous");
    }

    #[test]
    fn degraded_probe_advice_names_ca_cert_only_when_pinned() {
        assert_eq!(
            degraded_probe_advice(true),
            " Fix or remove ca_cert to restore authenticated probes."
        );
        assert_eq!(degraded_probe_advice(false), "");
    }

    #[tokio::test]
    async fn probe_client_without_credential_probes_unauthenticated_without_verification() {
        let dir = tempfile::tempdir().unwrap();
        let (_ca, url, _stop) = spawn_tls_peer(dir.path()).await;
        let shared: Arc<dyn io::HttpClient> = Arc::new(MockHttpClient::new());
        let client = build_probe_client(None, None, &shared);
        let response = client.get(&url).await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "anonymous");
    }

    #[tokio::test]
    async fn build_notifiers_creates_pushover_from_config() {
        let config = Config {
            notifiers: vec![config::NotifierConfig::Pushover {
                api_token: "tok".to_string(),
                user_key: "usr".to_string(),
                default_title: "Alert".to_string(),
                default_priority: 0,
                default_sound: "pushover".to_string(),
                api_url: None,
            }],
            ..Config::default()
        };
        let mock = MockHttpClient::new();
        let http: Arc<dyn io::HttpClient> = Arc::new(mock);

        let notifiers = config.build_notifiers(&http);

        assert_eq!(notifiers.len(), 1);
        assert_eq!(notifiers[0].type_name(), "pushover");
    }

    #[test]
    fn build_monitors_creates_alpaca_from_config() {
        let config = Config {
            monitors: vec![MonitorConfig::AlpacaSafetyMonitor {
                name: "Test Monitor".to_string(),
                host: "localhost".to_string(),
                port: 11111,
                device_number: 0,
                polling_interval: std::time::Duration::from_secs(30),
                scheme: "http".to_string(),
                auth: None,
            }],
            ..Config::default()
        };
        let mock = MockHttpClient::new();
        let http: Arc<dyn io::HttpClient> = Arc::new(mock);

        let monitors = config.build_monitors(&http, None);

        assert_eq!(monitors.len(), 1);
        assert_eq!(monitors[0].name(), "Test Monitor");
    }

    /// A no-op service manager so builder tests never touch the platform's
    /// real `systemctl`/SCM/brew.
    #[derive(Debug)]
    struct EmptyManager;

    #[async_trait::async_trait]
    impl ServiceManager for EmptyManager {
        async fn enumerate(&self) -> Result<Vec<discovery::DiscoveredUnit>> {
            Ok(Vec::new())
        }

        async fn restart(&self, _unit: &str, _budget: std::time::Duration) -> Result<()> {
            Ok(())
        }

        async fn recovery_check(&self, _unit: &str) -> Option<bool> {
            None
        }
    }

    fn empty_manager() -> Arc<dyn ServiceManager> {
        Arc::new(EmptyManager)
    }

    #[tokio::test]
    async fn builder_with_http_client_uses_injected_client() {
        let mock = MockHttpClient::new();
        let http: Arc<dyn io::HttpClient> = Arc::new(mock);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let sentinel = SentinelBuilder::new(Config::default())
            .with_http_client(http)
            .with_service_manager(empty_manager())
            .with_cancellation_token(cancel)
            .build()
            .await
            .unwrap();

        sentinel.start().await.unwrap();
    }

    #[tokio::test]
    async fn builder_with_monitors_skips_config_factory() {
        let cancel = CancellationToken::new();
        cancel.cancel();

        let config = Config {
            monitors: vec![MonitorConfig::AlpacaSafetyMonitor {
                name: "Should Be Ignored".to_string(),
                host: "localhost".to_string(),
                port: 11111,
                device_number: 0,
                polling_interval: std::time::Duration::from_secs(30),
                scheme: "http".to_string(),
                auth: None,
            }],
            ..Config::default()
        };

        let mock = MockHttpClient::new();
        let http: Arc<dyn io::HttpClient> = Arc::new(mock);

        // Injecting empty monitors should override the config factory
        let sentinel = SentinelBuilder::new(config)
            .with_http_client(http)
            .with_service_manager(empty_manager())
            .with_monitors(vec![])
            .with_cancellation_token(cancel)
            .build()
            .await
            .unwrap();

        sentinel.start().await.unwrap();
    }

    #[tokio::test]
    async fn builder_with_notifiers_skips_config_factory() {
        let cancel = CancellationToken::new();
        cancel.cancel();

        let config = Config {
            notifiers: vec![config::NotifierConfig::Pushover {
                api_token: "tok".to_string(),
                user_key: "usr".to_string(),
                default_title: "Alert".to_string(),
                default_priority: 0,
                default_sound: "pushover".to_string(),
                api_url: None,
            }],
            ..Config::default()
        };

        let mock = MockHttpClient::new();
        let http: Arc<dyn io::HttpClient> = Arc::new(mock);

        // Injecting empty notifiers should override the config factory
        let sentinel = SentinelBuilder::new(config)
            .with_http_client(http)
            .with_service_manager(empty_manager())
            .with_notifiers(vec![])
            .with_cancellation_token(cancel)
            .build()
            .await
            .unwrap();

        sentinel.start().await.unwrap();
    }

    #[tokio::test]
    async fn has_dashboard_true_when_enabled() {
        let mut config = Config::default();
        // Ephemeral port so parallel tests never contend for 11114.
        config.server.port = 0;
        let mock = MockHttpClient::new();
        let http: Arc<dyn io::HttpClient> = Arc::new(mock);

        let sentinel = SentinelBuilder::new(config)
            .with_http_client(http)
            .with_service_manager(empty_manager())
            .build()
            .await
            .unwrap();

        assert!(sentinel.has_dashboard());
    }

    #[tokio::test]
    async fn has_dashboard_false_when_disabled() {
        let config = Config {
            dashboard: config::DashboardConfig {
                enabled: false,
                ..config::DashboardConfig::default()
            },
            ..Config::default()
        };
        let mock = MockHttpClient::new();
        let http: Arc<dyn io::HttpClient> = Arc::new(mock);

        let sentinel = SentinelBuilder::new(config)
            .with_http_client(http)
            .with_service_manager(empty_manager())
            .build()
            .await
            .unwrap();

        assert!(!sentinel.has_dashboard());
    }
}

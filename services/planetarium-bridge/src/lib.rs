#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! planetarium-bridge
//!
//! Serves a **virtual ASCOM Alpaca Telescope** that planetarium apps
//! (`SkySafari`, Stellarium, Cartes du Ciel) connect to as if it were a
//! mount. Pressing **Align** imports the selected coordinates as a paused
//! target into rp's target store; slews are simulated motion and never
//! import. The service never touches hardware and is never on the imaging
//! path. Design: docs/services/planetarium-bridge.md.

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

pub mod config;
pub mod device;
pub mod doctor;
pub mod epoch;
pub mod import;
pub mod spool;

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ascom_alpaca::api::CargoServerInfo;
use ascom_alpaca::Server;
use rp_ephemeris::{Ephemeris, ErfarsEphemeris};
use rusty_photon_tls::config::TlsConfig;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::config::Config;
use crate::device::BridgeTelescope;
use crate::import::{ImportWorker, RpTarget};

/// Resolve the spool file location: the configured path, or the platform
/// default `<config root>/planetarium-bridge/spool.jsonl`.
///
/// # Errors
///
/// Returns an error if the platform config directory cannot be
/// determined (no resolvable home) or the spool directory cannot be
/// created.
pub fn resolve_spool_path(
    config: &Config,
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    if let Some(path) = &config.spool.path {
        return Ok(path.clone());
    }
    let dir = rusty_photon_config::default_config_dir()?.join("planetarium-bridge");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create spool directory {}: {e}", dir.display()))?;
    Ok(dir.join("spool.jsonl"))
}

/// Builder for the Alpaca server plus the import pipeline.
pub struct ServerBuilder {
    config: Config,
}

impl ServerBuilder {
    #[must_use]
    pub const fn new(config: Config) -> Self {
        Self { config }
    }

    /// Consume the builder: warm up ERFA, resolve the spool, start the
    /// import pipeline, and bind the configured listen address.
    ///
    /// # Errors
    ///
    /// Returns an error if the configured site coordinates are rejected
    /// (defensive — the config newtypes enforce the same ranges), the
    /// spool location cannot be resolved or created, the listener
    /// cannot be bound or its address read, or the config opts into
    /// Alpaca discovery and the responder's UDP port cannot be bound.
    pub async fn build(self) -> Result<BoundServer, Box<dyn std::error::Error + Send + Sync>> {
        let config = self.config;

        // Warm up ERFA's lazily-initialized tables single-threaded, before
        // concurrent Alpaca polls can race the first initialization.
        let ephemeris = ErfarsEphemeris::new();
        let site = rp_ephemeris::Site::new(
            config.site.site_latitude_deg.degrees(),
            config.site.site_longitude_deg.degrees(),
        )?;
        let lst = ephemeris.sidereal_time(&site, chrono::Utc::now());
        debug!(lst_hours = lst.lst_hours, "ERFA warmed up");

        let spool_path = resolve_spool_path(&config)?;
        debug!(spool = %spool_path.display(), "spool location resolved");
        let cancel = CancellationToken::new();
        let (importer, health, worker) = import::pipeline(
            spool_path,
            config.spool.max_entries.get(),
            config.spool.replay_backoff_max,
            RpTarget {
                mcp_server_url: config.rp.mcp_server_url.as_str().to_owned(),
                service_auth: config.rp.service_auth.clone(),
                ca_cert: config.rp.ca_cert.clone(),
            },
            cancel.clone(),
        );

        let last_client: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
        let telescope = BridgeTelescope::new(
            &config.device,
            &config.site,
            ephemeris,
            importer,
            Arc::clone(&last_client),
        )?;

        let mut server = Server::new(CargoServerInfo!());
        server.listen_addr = config.server.socket_addr();
        server.devices.register(telescope);
        info!("registered the virtual Telescope device");

        let health_route = {
            let health = Arc::clone(&health);
            axum::routing::get(move || {
                let health = Arc::clone(&health);
                async move { axum::Json(health.snapshot()) }
            })
        };
        let router = axum::Router::new()
            .route("/health", health_route)
            .fallback_service(server.into_service())
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&last_client),
                remember_client,
            ));

        // Layer authentication if configured.
        let router = match &config.server.auth {
            Some(auth) => {
                if config.server.tls.is_none() {
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

        let listener =
            rusty_photon_tls::server::bind_dual_stack_tokio(config.server.socket_addr()).await?;
        let local_addr = listener.local_addr()?;

        // Opt-in Alpaca UDP discovery responder (config `discovery_port`,
        // absent by default — plan Decision 1); bound here so a taken port
        // fails startup.
        let discovery =
            rusty_photon_driver::discovery::bind(local_addr, config.server.discovery_port).await?;

        // Console mode only: stdout is a dead handle under the Windows SCM,
        // and the only stdout consumer (bdd-infra's port parser) never runs
        // services with --service.
        if !rusty_photon_service_lifecycle::is_scm_service() {
            println!("Bound Alpaca server bound_addr={local_addr}");
        }
        info!("Bound Alpaca server bound_addr={local_addr}");

        Ok(BoundServer {
            listener,
            router,
            local_addr,
            tls: config.server.tls.clone(),
            discovery,
            worker,
            cancel,
        })
    }
}

/// Remember the peer address of every Alpaca request — the import
/// provenance's `client` field. Single-client in practice (P3a: one
/// planetarium drives the device).
async fn remember_client(
    axum::extract::State(last): axum::extract::State<Arc<Mutex<Option<SocketAddr>>>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Some(info) = request
        .extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
    {
        let mut guard = match last.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = Some(info.0);
    }
    next.run(request).await
}

/// A fully bound planetarium-bridge server ready to accept connections.
pub struct BoundServer {
    listener: tokio::net::TcpListener,
    router: axum::Router,
    local_addr: SocketAddr,
    tls: Option<TlsConfig>,
    discovery: Option<ascom_alpaca::discovery::BoundDiscoveryServer>,
    worker: ImportWorker,
    cancel: CancellationToken,
}

impl BoundServer {
    pub const fn listen_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serve the bound listener (and the discovery responder, when
    /// bound) until `shutdown` resolves, then stop the import worker.
    ///
    /// # Errors
    ///
    /// Returns an error if the TLS material cannot be loaded or the
    /// serve loop fails. Import-worker stop problems are logged, never
    /// returned — the spool keeps any undelivered backlog durable.
    pub async fn start(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        type ServeResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

        let Self {
            listener,
            router,
            local_addr,
            tls,
            discovery,
            worker,
            cancel,
        } = self;

        let worker_handle = tokio::spawn(worker.run());

        let serve = async {
            let result: ServeResult = if let Some(ref tls_config) = tls {
                // The TLS accept path serves the router without
                // per-connection info, so imports carry
                // `client: "unknown"` — acceptable: planetarium
                // clients speak plain HTTP (P3a).
                info!("planetarium-bridge started on {local_addr} (TLS)");
                rusty_photon_tls::server::serve_tls(listener, router, tls_config, shutdown)
                    .await
                    .map_err(Into::into)
            } else {
                info!("planetarium-bridge started on {local_addr}");
                axum::serve(
                    listener,
                    router.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .with_graceful_shutdown(shutdown)
                .await
                .map_err(Into::into)
            };
            result
        };
        let serve_result = rusty_photon_driver::discovery::serve_with(discovery, serve).await;

        // Stop the import worker after serving ends; the spool keeps any
        // undelivered backlog durable across the restart. The wait is
        // bounded so a wedged rp session can never hold the service's stop
        // hostage (the Windows SCM escalates a slow stop into an uninstall
        // failure) — an abandoned worker's backlog is already on disk.
        cancel.cancel();
        match tokio::time::timeout(std::time::Duration::from_secs(10), worker_handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("import worker task ended abnormally: {e}"),
            Err(_) => tracing::warn!("import worker did not stop within 10s; abandoning it"),
        }
        debug!("planetarium-bridge shut down");
        serve_result
    }
}

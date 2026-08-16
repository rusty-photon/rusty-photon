#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! Deep Sky Dad FP2 driver — ASCOM Alpaca `CoverCalibrator`.
//!
//! Wraps the FP2's bracketed ASCII serial protocol behind the workspace's
//! `rusty-photon-shared-transport` lifecycle scaffolding.

// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
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

pub mod codec;
pub mod config;
pub mod config_actions;
pub mod device;
pub mod doctor;
pub mod error;
pub mod manager;
#[cfg(feature = "mock")]
pub mod mock;
pub mod protocol;
pub mod transport;

pub use codec::Fp2Codec;
pub use config::{
    load_config, load_effective_config, AlpacaServerConfig, CliOverrides, Config,
    CoverCalibratorConfig, SerialConfig,
};
pub use device::DsdFp2Device;
pub use error::{DsdFp2Error, Result};
pub use manager::{CachedState, FlatPanelManager};
pub use transport::Fp2SerialTransportFactory;

#[cfg(feature = "mock")]
pub use mock::{MockState, MockTransportFactory};

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use ascom_alpaca::api::CargoServerInfo;
use ascom_alpaca::Server;
use rusty_photon_service_lifecycle::ReloadSignal;
use rusty_photon_shared_transport::TransportFactory;
use rusty_photon_tls::config::TlsConfig;
use tracing::{debug, info};

use crate::config_actions::DsdFp2Driver;

/// Builder for the FP2 ASCOM Alpaca server.
pub struct ServerBuilder {
    config: Config,
    factory: Arc<dyn TransportFactory>,
    /// Where `config.apply` persists and reload re-reads. `Some` enables the
    /// config actions (together with `reload`).
    config_path: Option<PathBuf>,
    /// CLI overrides, so config actions can distinguish file vs. override layers.
    overrides: CliOverrides,
    /// Reload trigger handed to the device for fire-after-response reload.
    reload: Option<ReloadSignal>,
}

fn default_factory(config: &Config) -> Arc<dyn TransportFactory> {
    let concrete = Fp2SerialTransportFactory::new(
        config.serial.port.clone(),
        config.serial.baud_rate,
        config.serial.timeout,
    );
    let factory: Arc<dyn TransportFactory> = Arc::new(concrete);
    factory
}

impl Default for ServerBuilder {
    fn default() -> Self {
        let config = Config::default();
        let factory = default_factory(&config);
        Self {
            config,
            factory,
            config_path: None,
            overrides: CliOverrides::default(),
            reload: None,
        }
    }
}

impl ServerBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_config(mut self, config: Config) -> Self {
        self.factory = default_factory(&config);
        self.config = config;
        self
    }

    /// Override the transport factory (used by mock-mode startup).
    #[must_use]
    pub fn with_factory(mut self, factory: Arc<dyn TransportFactory>) -> Self {
        self.factory = factory;
        self
    }

    /// Set the config source (persist path + CLI overrides) for the
    /// `config.get` / `config.apply` actions. Together with
    /// [`Self::with_reload_signal`], this enables config editing.
    #[must_use]
    pub fn with_config_source(mut self, path: PathBuf, overrides: CliOverrides) -> Self {
        self.config_path = Some(path);
        self.overrides = overrides;
        self
    }

    /// Provide the reload trigger `config.apply` fires after its response
    /// flushes. Together with [`Self::with_config_source`], this enables config
    /// editing.
    #[must_use]
    pub fn with_reload_signal(mut self, reload: ReloadSignal) -> Self {
        self.reload = Some(reload);
        self
    }

    pub async fn build(
        self,
    ) -> std::result::Result<BoundServer, Box<dyn std::error::Error + Send + Sync>> {
        let manager = FlatPanelManager::new(self.config.clone(), self.factory);

        // Eager hardware validation at startup: opens the port, runs the
        // handshake (identity probe), and spawns the reconnect supervisor before
        // binding the HTTP listener. On handshake failure the error bubbles up to
        // `main` for a non-zero exit, so systemd / orchestration treats startup
        // as failed rather than the service advertising a broken device on the
        // network.
        info!("validating hardware via eager startup handshake");
        manager.transport().start().await?;

        // Config-action wiring (persist path + CLI overrides + reload trigger),
        // cloned out here so the borrowing build block below can consume them.
        let config_path = self.config_path.clone();
        let overrides = self.overrides.clone();
        let reload = self.reload.clone();

        // All post-start work is fallible (bind / local_addr in particular).
        // Wrap it so a failure runs `transport.shutdown()` before propagating;
        // otherwise the reconnect supervisor task would outlive the dropped
        // manager and keep the port open until process exit.
        let build_result: std::result::Result<
            BoundServer,
            Box<dyn std::error::Error + Send + Sync>,
        > = async {
            let mut server = Server::new(CargoServerInfo!());
            server.listen_addr = self.config.server.socket_addr();

            if self.config.cover_calibrator.enabled {
                let mut device =
                    DsdFp2Device::new(self.config.cover_calibrator.clone(), Arc::clone(&manager));
                let config_ctx: Option<rusty_photon_driver::ConfigActionCtx<DsdFp2Driver>> =
                    match (config_path.clone(), reload.clone()) {
                        (Some(path), Some(reload)) => Some(rusty_photon_driver::ConfigActionCtx {
                            effective: self.config.clone(),
                            path,
                            overrides: overrides.clone(),
                            reload,
                        }),
                        _ => None,
                    };
                if let Some(ctx) = config_ctx {
                    device = device.with_config_actions(ctx);
                }
                server.devices.register(device);
                info!(
                    "Registered CoverCalibrator device: {}",
                    self.config.cover_calibrator.name
                );
            }

            info!("Serial port: {}", self.config.serial.port);

            let tls = self.config.server.tls.clone();
            let router = axum::Router::new().fallback_service(server.into_service());

            let router = match &self.config.server.auth {
                Some(auth) => {
                    if self.config.server.tls.is_none() {
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
                rusty_photon_tls::server::bind_dual_stack_tokio(self.config.server.socket_addr())
                    .await?;
            let local_addr = listener.local_addr()?;

            // Opt-in Alpaca UDP discovery responder (config `discovery_port`);
            // bound here so a taken port fails startup, run in start().
            let discovery =
                rusty_photon_driver::discovery::bind(local_addr, self.config.server.discovery_port)
                    .await?;

            // Console mode only: stdout is a dead handle under the Windows SCM,
            // and the only stdout consumer (bdd-infra's port parser) never runs
            // services with --service.
            if !rusty_photon_service_lifecycle::is_scm_service() {
                println!("Bound Alpaca server bound_addr={local_addr}");
            }
            info!("Bound Alpaca server bound_addr={}", local_addr);

            Ok(BoundServer {
                listener,
                router,
                local_addr,
                tls,
                discovery,
                manager: Arc::clone(&manager),
            })
        }
        .await;

        match build_result {
            Ok(bound) => Ok(bound),
            Err(e) => {
                if let Err(shutdown_err) = manager.transport().shutdown().await {
                    tracing::warn!(
                        error = %shutdown_err,
                        "transport shutdown failed during build() error rollback"
                    );
                }
                Err(e)
            }
        }
    }
}

/// Fully bound FP2 server ready to accept connections.
pub struct BoundServer {
    listener: tokio::net::TcpListener,
    router: axum::Router,
    local_addr: SocketAddr,
    tls: Option<TlsConfig>,
    /// Alpaca UDP discovery responder, when the config opts in. Runs inside
    /// `start()`'s select so its socket closes when serving ends (reload).
    discovery: Option<ascom_alpaca::discovery::BoundDiscoveryServer>,
    /// Held so `start()` can call `manager.transport().shutdown()` after the HTTP
    /// server stops — on shutdown *and* on reload (`main` awaits `start()` to
    /// completion rather than dropping it), so the serial port and reconnect
    /// supervisor are released before the service rebinds.
    manager: Arc<FlatPanelManager>,
}

impl BoundServer {
    pub const fn listen_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn start(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let Self {
            listener,
            router,
            local_addr,
            tls,
            discovery,
            manager,
        } = self;
        // Capture the serve result so transport.shutdown() runs even when the
        // HTTP server errors out — otherwise the supervisor and port would leak
        // past a serve failure.
        let serve = async {
            if let Some(ref tls_config) = tls {
                info!("dsd-fp2 started on {} (TLS)", local_addr);
                rusty_photon_tls::server::serve_tls(listener, router, tls_config, shutdown).await
            } else {
                info!("dsd-fp2 started on {}", local_addr);
                rusty_photon_tls::server::serve_plain(listener, router, shutdown).await
            }
        };
        let serve_result = rusty_photon_driver::discovery::serve_with(discovery, serve).await;
        if let Err(e) = manager.transport().shutdown().await {
            tracing::warn!(error = %e, "transport shutdown returned an error during teardown");
        }
        debug!("dsd-fp2 shut down");
        serve_result.map_err(Into::into)
    }
}

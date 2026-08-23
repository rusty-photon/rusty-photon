#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
//! Pegasus Astro Scops OAG Driver
//!
//! ASCOM Alpaca Focuser driver for the Pegasus Astro Scops OAG (motorized
//! off-axis guider focuser).
//!
//! Exposes one ASCOM Focuser device over a shared serial transport managed by
//! `rusty_photon_shared_transport::SharedTransport`. The per-service lifecycle
//! scaffolding (refcount, slot, polling task, command-lock arbitration) lives in
//! the shared crate; this service contributes the [`ScopsCodec`], the
//! [`FocuserManager`] hooks, and the ASCOM trait implementation.

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
pub mod doctor;
pub mod error;
pub mod focuser_device;
pub mod manager;
// Compiled into the binary under `--features mock` (BDD + ConformU) and also
// into the lib's `cargo test` build so each module's `#[cfg(test)]` suite can
// drive the same canonical mock simulator. Production builds don't compile it.
#[cfg(any(feature = "mock", test))]
pub mod mock;
pub mod protocol;
pub mod serial;

pub use codec::{ScopsCodec, ScopsCodecError, ScopsResponse};
pub use config::{load_config, AlpacaServerConfig, Config, FocuserConfig, SerialConfig};
pub use error::{Result, ScopsOagError};
pub use focuser_device::ScopsFocuserDevice;
pub use manager::{CachedState, FocuserManager};
pub use serial::ScopsTransportFactory;

#[cfg(feature = "mock")]
pub use mock::MockScopsTransportFactory;

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

use crate::config::CliOverrides;
use crate::config_actions::ScopsFocuserDriver;

/// Builder for the ASCOM Alpaca server.
#[derive(Default)]
pub struct ServerBuilder {
    config: Config,
    factory: Option<Arc<dyn TransportFactory>>,
    /// Where `config.apply` persists + which fields are CLI-pinned. `Some`
    /// enables the config actions on the registered device.
    config_source: Option<(PathBuf, CliOverrides)>,
    /// In-process reload trigger handed to the device's `config.apply` handler.
    reload: Option<ReloadSignal>,
}

impl ServerBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    #[must_use]
    pub fn with_factory(mut self, factory: Arc<dyn TransportFactory>) -> Self {
        self.factory = Some(factory);
        self
    }

    /// Wire the config-action source (persist path + CLI overrides) so the
    /// registered focuser device advertises `config.get` / `config.apply` /
    /// `config.schema`.
    #[must_use]
    pub fn with_config_source(mut self, path: PathBuf, overrides: CliOverrides) -> Self {
        self.config_source = Some((path, overrides));
        self
    }

    /// Hand the device the in-process reload trigger fired after a `config.apply`
    /// that needs a reload.
    #[must_use]
    pub fn with_reload_signal(mut self, reload: ReloadSignal) -> Self {
        self.reload = Some(reload);
        self
    }

    /// Validate the hardware with the eager startup handshake (on the
    /// injected factory, else the serial one the config describes), register
    /// the focuser device, and bind the listener.
    ///
    /// # Errors
    ///
    /// Returns the transport's
    /// [`SessionError`](rusty_photon_shared_transport::SessionError) if the
    /// transport cannot be opened (the configured serial port, unless a
    /// factory was injected) or the handshake fails, and the I/O error
    /// if the listener or the opted-in discovery responder cannot be bound (or
    /// the bound address read) — the already-started transport is shut down
    /// again before that error is returned.
    pub async fn build(
        self,
    ) -> std::result::Result<BoundServer, Box<dyn std::error::Error + Send + Sync>> {
        // Default to the real-hardware factory when none was supplied.
        let factory: Arc<dyn TransportFactory> = self.factory.unwrap_or_else(|| {
            Arc::new(ScopsTransportFactory::new(
                self.config.serial.port.clone(),
                self.config.serial.baud_rate,
                self.config.serial.timeout,
            ))
        });

        let manager = FocuserManager::new(&self.config, factory);

        // Eager hardware validation at startup: open the port, run the
        // handshake, and spawn the reconnect supervisor before binding the HTTP
        // listener. Handshake failures bubble up to `main` for a non-zero exit.
        info!("validating hardware via eager startup handshake");
        manager.transport().start().await?;

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

            if self.config.focuser.enabled {
                let mut focuser_device =
                    ScopsFocuserDevice::new(self.config.focuser.clone(), Arc::clone(&manager));
                let config_ctx: Option<rusty_photon_driver::ConfigActionCtx<ScopsFocuserDriver>> =
                    match (self.config_source.clone(), self.reload.clone()) {
                        (Some((path, overrides)), Some(reload)) => {
                            Some(rusty_photon_driver::ConfigActionCtx {
                                effective: self.config.clone(),
                                path,
                                overrides,
                                reload,
                            })
                        }
                        _ => None,
                    };
                if let Some(ctx) = config_ctx {
                    focuser_device = focuser_device.with_config_actions(ctx);
                }
                server.devices.register(focuser_device);
                info!("Registered Focuser device: {}", self.config.focuser.name);
            }

            info!("Serial port: {}", self.config.serial.port);

            let tls = self.config.server.tls.clone();
            let router = axum::Router::new().fallback_service(server.into_service());

            // Layer authentication if configured
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

            // This println is parsed by tests to discover the bound port. It
            // must go to stdout (not tracing/stderr) so the subprocess output
            // can be read.
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

/// A fully bound pa-scops-oag server ready to accept connections.
pub struct BoundServer {
    listener: tokio::net::TcpListener,
    router: axum::Router,
    local_addr: SocketAddr,
    tls: Option<TlsConfig>,
    /// Alpaca UDP discovery responder, when the config opts in. Runs inside
    /// `start()`'s select so its socket closes when serving ends (reload).
    discovery: Option<ascom_alpaca::discovery::BoundDiscoveryServer>,
    /// Held so `start()` can call `manager.transport().shutdown()` after the
    /// HTTP server stops.
    manager: Arc<FocuserManager>,
}

impl BoundServer {
    pub const fn listen_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serve until `shutdown` resolves, then shut the transport down.
    ///
    /// # Errors
    ///
    /// Returns the serve error if the TLS material cannot be loaded or the
    /// serve loop fails; a transport-shutdown failure during teardown is
    /// logged, not returned.
    pub async fn start(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Capture the serve result so transport.shutdown() runs even when the
        // HTTP server errors out — otherwise the supervisor and port would leak.
        let Self {
            listener,
            router,
            local_addr,
            tls,
            discovery,
            manager,
        } = self;
        let serve = async {
            if let Some(ref tls_config) = tls {
                info!("pa-scops-oag started on {} (TLS)", local_addr);
                rusty_photon_tls::server::serve_tls(listener, router, tls_config, shutdown).await
            } else {
                info!("pa-scops-oag started on {}", local_addr);
                rusty_photon_tls::server::serve_plain(listener, router, shutdown).await
            }
        };
        let serve_result = rusty_photon_driver::discovery::serve_with(discovery, serve).await;
        if let Err(e) = manager.transport().shutdown().await {
            tracing::warn!(error = %e, "transport shutdown returned an error during teardown");
        }
        debug!("pa-scops-oag shut down");
        serve_result.map_err(Into::into)
    }
}

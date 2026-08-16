#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
//! Pegasus Falcon Rotator Driver
//!
//! ASCOM Alpaca driver exposing the Pegasus Astro Falcon Rotator
//! (firmware 1.3 or newer) as two devices on one server: an `IRotatorV4`
//! for motion and an `ISwitchV3` for status (raw input voltage +
//! limit-detect flag).
//!
//! See `docs/services/falcon-rotator.md` for the behavioural contract.
//! Lifecycle scaffolding is shared with `qhy-focuser`, `ppba-driver`, and
//! `star-adventurer-gti` via
//! [`rusty_photon_shared_transport::SharedTransport`].

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
pub mod manager;
#[cfg(feature = "mock")]
pub mod mock;
pub mod protocol;
pub mod rotator_device;
pub mod serial;
pub mod switch_device;
pub mod units;

pub use codec::{FalconCodec, FalconCodecError, FalconResponse};
pub use config::{
    load_config, AlpacaServerConfig, Config, RotatorConfig, SerialConfig, SwitchConfig,
};
pub use error::{FalconRotatorError, Result};
pub use manager::FalconManager;
pub use rotator_device::FalconRotatorDevice;
pub use serial::FalconTransportFactory;
pub use switch_device::FalconStatusSwitchDevice;
pub use units::{MechanicalDegrees, SkyDegrees, Steps, SyncOffset};

#[cfg(feature = "mock")]
pub use mock::MockFalconTransportFactory;

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
use crate::config_actions::FalconRotatorDriver;

/// Builder for the ASCOM Alpaca server.
///
/// Wires the Rotator and Status Switch devices through a single
/// [`FalconManager`] so they share one
/// [`rusty_photon_shared_transport::SharedTransport`] and therefore one
/// physical serial connection.
pub struct ServerBuilder {
    config: Config,
    factory: Arc<dyn TransportFactory>,
    /// Where `config.apply` persists + which fields are CLI-pinned. `Some`
    /// enables the config actions on both registered devices.
    config_source: Option<(PathBuf, CliOverrides)>,
    /// In-process reload trigger handed to the devices' `config.apply` handler.
    reload: Option<ReloadSignal>,
}

impl Default for ServerBuilder {
    fn default() -> Self {
        let factory = FalconTransportFactory::from_config(&Config::default().serial);
        Self {
            config: Config::default(),
            factory: Arc::new(factory),
            config_source: None,
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
        // Rebuild the default factory from the new config so the
        // builder's factory always reflects the configured serial port
        // when the caller doesn't supply one explicitly.
        let factory = FalconTransportFactory::from_config(&config.serial);
        self.factory = Arc::new(factory);
        self.config = config;
        self
    }

    #[must_use]
    pub fn with_factory(mut self, factory: Arc<dyn TransportFactory>) -> Self {
        self.factory = factory;
        self
    }

    /// Wire the config-action source (persist path + CLI overrides) so both
    /// registered devices advertise `config.get` / `config.apply` /
    /// `config.schema`.
    #[must_use]
    pub fn with_config_source(mut self, path: PathBuf, overrides: CliOverrides) -> Self {
        self.config_source = Some((path, overrides));
        self
    }

    /// Hand the devices the in-process reload trigger fired after a `config.apply`
    /// that needs a reload.
    #[must_use]
    pub fn with_reload_signal(mut self, reload: ReloadSignal) -> Self {
        self.reload = Some(reload);
        self
    }

    pub async fn build(
        self,
    ) -> std::result::Result<BoundServer, Box<dyn std::error::Error + Send + Sync>> {
        let manager = FalconManager::new(self.factory);

        // Eager hardware validation at startup: opens the port,
        // runs the handshake, and spawns the reconnect supervisor
        // before binding the HTTP listener. Handshake failures
        // bubble up to `main` for a non-zero exit.
        info!("validating hardware via eager startup handshake");
        manager.transport().start().await?;

        // All post-start work is fallible (bind / local_addr in
        // particular). Wrap it so a failure runs `transport.shutdown()`
        // before propagating; otherwise the reconnect supervisor task
        // would outlive the dropped manager and keep the port open
        // until process exit.
        let build_result: std::result::Result<
            BoundServer,
            Box<dyn std::error::Error + Send + Sync>,
        > = async {
            let mut server = Server::new(CargoServerInfo!());
            server.listen_addr = self.config.server.socket_addr();

            // Build the shared config-action context once (when a config source
            // + reload signal were supplied) and clone it to each device, so both
            // advertise the actions against the one driver config + reload signal.
            let config_ctx: Option<rusty_photon_driver::ConfigActionCtx<FalconRotatorDriver>> =
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

            if self.config.rotator.enabled {
                let mut rotator_device =
                    FalconRotatorDevice::new(self.config.rotator.clone(), Arc::clone(&manager));
                if let Some(ctx) = config_ctx.clone() {
                    rotator_device = rotator_device.with_config_actions(ctx);
                }
                server.devices.register(rotator_device);
                info!("Registered Rotator device: {}", self.config.rotator.name);
            }

            if self.config.switch.enabled {
                let mut switch_device =
                    FalconStatusSwitchDevice::new(self.config.switch.clone(), Arc::clone(&manager));
                if let Some(ctx) = config_ctx.clone() {
                    switch_device = switch_device.with_config_actions(ctx);
                }
                server.devices.register(switch_device);
                info!(
                    "Registered Status Switch device: {}",
                    self.config.switch.name
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

/// A fully bound pa-falcon-rotator server ready to accept connections.
pub struct BoundServer {
    listener: tokio::net::TcpListener,
    router: axum::Router,
    local_addr: SocketAddr,
    tls: Option<TlsConfig>,
    /// Alpaca UDP discovery responder, when the config opts in. Runs inside
    /// `start()`'s select so its socket closes when serving ends (reload).
    discovery: Option<ascom_alpaca::discovery::BoundDiscoveryServer>,
    /// Held so `start()` can call `manager.transport().shutdown()` after
    /// the HTTP server stops. No-op in `LazyAcquire` mode.
    manager: Arc<FalconManager>,
}

impl BoundServer {
    pub const fn listen_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn start(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Capture the serve result so transport.shutdown() runs even
        // when the HTTP server errors out — otherwise the supervisor
        // and port would leak past a serve failure.
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
                info!("pa-falcon-rotator started on {} (TLS)", local_addr);
                rusty_photon_tls::server::serve_tls(listener, router, tls_config, shutdown).await
            } else {
                info!("pa-falcon-rotator started on {}", local_addr);
                rusty_photon_tls::server::serve_plain(listener, router, shutdown).await
            }
        };
        let serve_result = rusty_photon_driver::discovery::serve_with(discovery, serve).await;
        if let Err(e) = manager.transport().shutdown().await {
            tracing::warn!(error = %e, "transport shutdown returned an error during teardown");
        }
        debug!("pa-falcon-rotator shut down");
        serve_result.map_err(Into::into)
    }
}

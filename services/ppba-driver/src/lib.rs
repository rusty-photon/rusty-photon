#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
//! PPBA Driver
//!
//! ASCOM Alpaca driver for the Pegasus Astro Pocket Powerbox Advance Gen2 (PPBA).
//!
//! Exposes two ASCOM devices over one shared serial transport managed by
//! `rusty_photon_shared_transport::SharedTransport`:
//! - Switch device (power control + sensor monitoring)
//! - `ObservingConditions` device (environmental sensors)

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
pub mod mean;
// Compiled into the binary under `--features mock` (BDD + ConformU) and
// also into the lib's `cargo test` build so each module's `#[cfg(test)]`
// suite can drive the same canonical PPBA simulator. Production builds
// don't compile it.
#[cfg(any(feature = "mock", test))]
pub mod mock;
pub mod observingconditions_device;
pub mod protocol;
pub mod serial;
pub mod switch_device;
pub mod switches;

pub use codec::{PpbaCodec, PpbaCodecError, PpbaResponse};
pub use config::{
    load_config, AlpacaServerConfig, Config, DeviceConfig, ObservingConditionsConfig, SerialConfig,
    SwitchConfig,
};
pub use error::{PpbaError, Result};
pub use manager::{CachedState, PpbaManager};
pub use observingconditions_device::PpbaObservingConditionsDevice;
pub use serial::PpbaTransportFactory;
pub use switch_device::PpbaSwitchDevice;
pub use switches::{SwitchId, SwitchInfo, MAX_SWITCH};

#[cfg(feature = "mock")]
pub use mock::MockPpbaTransportFactory;

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
use crate::config_actions::PpbaDriver;

/// Builder for the ASCOM Alpaca server.
pub struct ServerBuilder {
    config: Config,
    factory: Arc<dyn TransportFactory>,
    /// Where `config.apply` persists + which fields are CLI-pinned. `Some`
    /// enables the config actions on both registered devices.
    config_source: Option<(PathBuf, CliOverrides)>,
    /// In-process reload trigger handed to the devices' `config.apply` handler.
    reload: Option<ReloadSignal>,
}

impl ServerBuilder {
    #[must_use]
    pub fn new(config: Config) -> Self {
        let factory: Arc<dyn TransportFactory> = Arc::new(PpbaTransportFactory::new(
            config.serial.port.clone(),
            config.serial.baud_rate,
            config.serial.timeout,
        ));
        Self {
            config,
            factory,
            config_source: None,
            reload: None,
        }
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
        let manager = PpbaManager::new(self.config.clone(), self.factory);

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
            let config_ctx: Option<rusty_photon_driver::ConfigActionCtx<PpbaDriver>> =
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

            if self.config.switch.enabled {
                let mut switch_device =
                    PpbaSwitchDevice::new(self.config.switch.clone(), Arc::clone(&manager));
                if let Some(ctx) = config_ctx.clone() {
                    switch_device = switch_device.with_config_actions(ctx);
                }
                server.devices.register(switch_device);
                info!("Registered Switch device: {}", self.config.switch.name);
            }

            if self.config.observingconditions.enabled {
                let mut oc_device = PpbaObservingConditionsDevice::new(
                    self.config.observingconditions.clone(),
                    Arc::clone(&manager),
                );
                if let Some(ctx) = config_ctx.clone() {
                    oc_device = oc_device.with_config_actions(ctx);
                }
                server.devices.register(oc_device);
                info!(
                    "Registered ObservingConditions device: {}",
                    self.config.observingconditions.name
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

            // This println is parsed by conformu_integration tests to discover the bound port.
            // It must go to stdout (not tracing/stderr) so the subprocess output can be read.
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
    manager: Arc<PpbaManager>,
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
        // Capture the serve result so transport.shutdown() runs even
        // when the HTTP server errors out — otherwise the supervisor
        // and port would leak past a serve failure.
        let serve = async {
            if let Some(ref tls_config) = tls {
                info!("ppba-driver started on {} (TLS)", local_addr);
                rusty_photon_tls::server::serve_tls(listener, router, tls_config, shutdown).await
            } else {
                info!("ppba-driver started on {}", local_addr);
                rusty_photon_tls::server::serve_plain(listener, router, shutdown).await
            }
        };
        let serve_result = rusty_photon_driver::discovery::serve_with(discovery, serve).await;
        if let Err(e) = manager.transport().shutdown().await {
            tracing::warn!(error = %e, "transport shutdown returned an error during teardown");
        }
        debug!("ppba-driver shut down");
        serve_result.map_err(Into::into)
    }
}

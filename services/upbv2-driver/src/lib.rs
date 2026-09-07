#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
//! UPBv2 Driver
//!
//! ASCOM Alpaca driver for the Pegasus Astro Ultimate Powerbox v2 (UPBv2),
//! served on port 11127 by default.
//!
//! Exposes two ASCOM devices over one shared serial transport managed by
//! `rusty_photon_shared_transport::SharedTransport`:
//! - Switch device — 39 switches covering the four 12 V outputs, three dew
//!   channels, the variable-voltage output, six USB ports, and the per-channel
//!   current, overcurrent and power-counter telemetry
//! - `ObservingConditions` device (temperature, humidity, dewpoint)
//!
//! The UPBv2's onboard stepper driver is **not** exposed: the Focuser device is
//! deferred until there is a motor on a fleet UPBv2 to validate it against. See
//! `docs/services/upbv2-driver.md` for the switch table and the deferral.
//!
//! This is a separate service from `ppba-driver`, not a mode of it — the two
//! boxes share a vendor, a USB id and a serial framing but not a command
//! language, and `P3:`/`P4:` mean different things on each.

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

pub mod codec;
pub mod config;
pub mod config_actions;
pub mod doctor;
pub mod error;
pub mod manager;
pub mod mean;
// Compiled into the binary under `--features mock` (BDD + ConformU) and
// also into the lib's `cargo test` build so each module's `#[cfg(test)]`
// suite can drive the same canonical UPBv2 simulator. Production builds
// don't compile it.
#[cfg(any(feature = "mock", test))]
pub mod mock;
pub mod observingconditions_device;
pub mod protocol;
pub mod serial;
pub mod switch_device;
pub mod switches;

pub use codec::{Upbv2Codec, Upbv2CodecError, Upbv2Response};
pub use config::{
    load_config, AlpacaServerConfig, Config, DeviceConfig, ObservingConditionsConfig, SerialConfig,
    SwitchConfig,
};
pub use error::{Result, Upbv2Error};
pub use manager::{CachedState, Upbv2Manager};
pub use observingconditions_device::Upbv2ObservingConditionsDevice;
pub use serial::Upbv2TransportFactory;
pub use switch_device::Upbv2SwitchDevice;
pub use switches::{SwitchId, SwitchInfo, MAX_SWITCH};

#[cfg(feature = "mock")]
pub use mock::MockUpbv2TransportFactory;

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
use crate::config_actions::Upbv2Driver;

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
        let factory: Arc<dyn TransportFactory> = Arc::new(Upbv2TransportFactory::new(
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

    /// Validate the hardware with the eager startup handshake, register the
    /// enabled devices, and bind the listener.
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
        let manager = Upbv2Manager::new(&self.config, self.factory);

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
            let config_ctx: Option<rusty_photon_driver::ConfigActionCtx<Upbv2Driver>> =
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
                    Upbv2SwitchDevice::new(self.config.switch.clone(), Arc::clone(&manager));
                if let Some(ctx) = config_ctx.clone() {
                    switch_device = switch_device.with_config_actions(ctx);
                }
                server.devices.register(switch_device);
                info!("Registered Switch device: {}", self.config.switch.name);
            }

            if self.config.observingconditions.enabled {
                let mut oc_device = Upbv2ObservingConditionsDevice::new(
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
    manager: Arc<Upbv2Manager>,
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
                info!("upbv2-driver started on {} (TLS)", local_addr);
                rusty_photon_tls::server::serve_tls(listener, router, tls_config, shutdown).await
            } else {
                info!("upbv2-driver started on {}", local_addr);
                rusty_photon_tls::server::serve_plain(listener, router, shutdown).await
            }
        };
        let serve_result = rusty_photon_driver::discovery::serve_with(discovery, serve).await;
        if let Err(e) = manager.transport().shutdown().await {
            tracing::warn!(error = %e, "transport shutdown returned an error during teardown");
        }
        debug!("upbv2-driver shut down");
        serve_result.map_err(Into::into)
    }
}

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
//! Gemini Power & Data Hubs Advanced 3 Driver
//!
//! ASCOM Alpaca driver for the Gemini Astro Power & Data Hubs Advanced 3,
//! served on port 11132 by default.
//!
//! Exposes two ASCOM devices over one shared serial transport managed by
//! `rusty_photon_shared_transport::SharedTransport`:
//! - Switch device — 26 switches: the always-on DC1 and the four switched
//!   12 V outputs, six USB ports, two dew channels each as duty / enabled /
//!   mode, the voltage / current / power telemetry, the two sensors' readings
//!   and their attached flags
//! - `ObservingConditions` device (the AHT20's temperature, humidity and
//!   dewpoint)
//!
//! The hub streams its status frame every 3 s once asked; this driver does
//! not consume that stream as its poll, it discards it and asks for a fresh
//! frame, and every write is confirmed against the frame that follows it.
//! See `docs/services/gemini-powerbox-adv3-driver.md` for the switch table,
//! the wire measurements and the reasons.
//!
//! This is a separate service from the two Pegasus drivers: the command
//! language is Gemini's own `>X#` family, over a CH340 at 19200 baud.

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
pub mod doctor;
pub mod error;
pub mod manager;
// Compiled into the binary under `--features mock` (BDD + ConformU) and
// also into the lib's `cargo test` build so each module's `#[cfg(test)]`
// suite can drive the same canonical Gemini hub simulator. Production builds
// don't compile it.
#[cfg(any(feature = "mock", test))]
pub mod mock;
pub mod observingconditions_device;
pub mod protocol;
pub mod serial;
pub mod setup;
pub mod switch_device;
pub mod switches;

pub use codec::{GeminiCodec, GeminiCodecError, GeminiResponse};
pub use config::{
    AlpacaServerConfig, Config, ObservingConditionsConfig, SerialConfig, SwitchConfig,
};
pub use error::{GeminiError, Result};
pub use manager::{CachedState, GeminiManager};
pub use observingconditions_device::GeminiObservingConditionsDevice;
pub use serial::GeminiTransportFactory;
pub use switch_device::GeminiSwitchDevice;
pub use switches::{SwitchId, SwitchInfo, MAX_SWITCH};

#[cfg(feature = "mock")]
pub use mock::MockGeminiTransportFactory;

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use ascom_alpaca::api::CargoServerInfo;
use ascom_alpaca::Server;
use rusty_photon_service_lifecycle::ReloadSignal;
use rusty_photon_setup::SetupPages;
use rusty_photon_shared_transport::TransportFactory;
use rusty_photon_tls::config::TlsConfig;
use tracing::{debug, info};

use crate::config::CliOverrides;
use crate::setup::GeminiDriver;

/// The setup pages: one handler behind the server page and both device
/// pages, present when the builder was given a config source and a reload
/// signal (the normal path through `main`).
fn setup_pages(
    config: &Config,
    config_source: Option<(PathBuf, CliOverrides)>,
    reload: Option<ReloadSignal>,
) -> Option<Arc<SetupPages<GeminiDriver>>> {
    match (config_source, reload) {
        (Some((path, overrides)), Some(reload)) => Some(Arc::new(
            SetupPages::<GeminiDriver>::new(
                "gemini-powerbox-adv3-driver",
                config.clone(),
                path,
                overrides,
            )
            .with_reload(reload)
            .with_https(config.server.tls.is_some()),
        )),
        _ => None,
    }
}

/// Builder for the ASCOM Alpaca server.
pub struct ServerBuilder {
    config: Config,
    factory: Arc<dyn TransportFactory>,
    /// Where a setup-page save persists + which fields are CLI-pinned.
    /// `Some` enables the setup pages (together with `reload`).
    config_source: Option<(PathBuf, CliOverrides)>,
    /// Reload trigger the pages fire after a save's response has flushed.
    reload: Option<ReloadSignal>,
}

impl ServerBuilder {
    #[must_use]
    pub fn new(config: Config) -> Self {
        let factory: Arc<dyn TransportFactory> = Arc::new(GeminiTransportFactory::new(
            config.serial.port.clone(),
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

    /// Set the config source (persist path + CLI overrides) the setup pages
    /// save to. Together with [`Self::with_reload_signal`], this enables
    /// config editing.
    #[must_use]
    pub fn with_config_source(mut self, path: PathBuf, overrides: CliOverrides) -> Self {
        self.config_source = Some((path, overrides));
        self
    }

    /// Provide the reload trigger a setup-page save fires after its response
    /// flushes. Together with [`Self::with_config_source`], this enables config
    /// editing.
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
        let manager = GeminiManager::new(&self.config, self.factory);

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

            let pages = setup_pages(
                &self.config,
                self.config_source.clone(),
                self.reload.clone(),
            );
            if let Some(pages) = &pages {
                server.setup = Some(Arc::new(rusty_photon_driver::ServerPages(Arc::clone(
                    pages,
                ))));
            }

            if self.config.switch.enabled {
                let mut switch_device =
                    GeminiSwitchDevice::new(self.config.switch.clone(), Arc::clone(&manager));
                if let Some(pages) = &pages {
                    switch_device = switch_device.with_setup_pages(Arc::clone(pages));
                }
                server.devices.register(switch_device);
                info!("Registered Switch device: {}", self.config.switch.name);
            }

            if self.config.observingconditions.enabled {
                let mut oc_device = GeminiObservingConditionsDevice::new(
                    self.config.observingconditions.clone(),
                    Arc::clone(&manager),
                );
                if let Some(pages) = &pages {
                    oc_device = oc_device.with_setup_pages(Arc::clone(pages));
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
    manager: Arc<GeminiManager>,
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
                info!(
                    "gemini-powerbox-adv3-driver started on {} (TLS)",
                    local_addr
                );
                rusty_photon_tls::server::serve_tls(listener, router, tls_config, shutdown).await
            } else {
                info!("gemini-powerbox-adv3-driver started on {}", local_addr);
                rusty_photon_tls::server::serve_plain(listener, router, shutdown).await
            }
        };
        let serve_result = rusty_photon_driver::discovery::serve_with(discovery, serve).await;
        if let Err(e) = manager.transport().shutdown().await {
            tracing::warn!(error = %e, "transport shutdown returned an error during teardown");
        }
        debug!("gemini-powerbox-adv3-driver shut down");
        serve_result.map_err(Into::into)
    }
}

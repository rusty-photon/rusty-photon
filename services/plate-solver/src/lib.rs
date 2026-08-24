#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
//! plate-solver — rp-managed service wrapping the ASTAP CLI.
//!
//! See `docs/services/plate-solver.md` for the design contract and
//! `docs/plans/archive/plate-solver.md` for sequencing.

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

pub mod api;
pub mod config;
pub mod doctor;
pub mod error;
pub mod runner;
pub mod supervision;

pub use api::AppState;
pub use config::{load_config, Config, ConfigError};
pub use error::{AppError, ErrorCode, ErrorResponse};
pub use runner::astap::AstapCliRunner;
pub use runner::{AstapRunner, RunnerError, SolveOutcome, SolveRequest, WcsMatrix};

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

/// Two-phase server builder.
///
/// Use `build()` to bind the TCP listener (so the bound port is known
/// up-front), then `start()` to serve. Mirrors `services/rp::ServerBuilder`
/// and avoids the port-TOCTOU race noted in
/// `docs/skills/development-workflow.md` §"Phase 4 Stabilization".
pub struct ServerBuilder {
    config: Option<Config>,
    runner: Option<Arc<dyn AstapRunner>>,
}

impl ServerBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: None,
            runner: None,
        }
    }

    #[must_use]
    pub fn with_config(mut self, config: Config) -> Self {
        self.config = Some(config);
        self
    }

    /// Override the runner (tests inject mocks; production uses
    /// `AstapCliRunner` constructed from config).
    #[must_use]
    pub fn with_runner(mut self, runner: Arc<dyn AstapRunner>) -> Self {
        self.runner = Some(runner);
        self
    }

    /// Consume the builder, validate the config, and bind the
    /// configured listen address.
    ///
    /// # Errors
    ///
    /// Returns an error if no config was supplied, the config fails
    /// [`Config::validate`] (surfaced as an `other` I/O error), or the
    /// listener cannot be bound or its address read.
    pub async fn build(self) -> Result<BoundServer, std::io::Error> {
        let config = self.config.ok_or_else(|| {
            std::io::Error::other(
                "ServerBuilder::build: config is required \u{2014} call .with_config(...) first",
            )
        })?;
        config
            .validate()
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let runner = self.runner.unwrap_or_else(|| {
            let mut runner = AstapCliRunner::new(
                config.astap_binary_path.clone(),
                config.astap_db_directory.clone(),
            );
            for (k, v) in &config.astap_extra_env {
                runner = runner.with_env(k, v);
            }
            Arc::new(runner)
        });

        let semaphore = Arc::new(Semaphore::new(config.max_concurrency));

        let state = AppState {
            runner,
            semaphore,
            default_solve_timeout: config.default_solve_timeout,
            max_solve_timeout: config.max_solve_timeout,
            astap_binary_path: config.astap_binary_path.clone(),
            astap_db_directory: config.astap_db_directory.clone(),
        };

        let router = api::build_router(state);

        // Opt-in HTTP Basic Auth (shared server config `auth` block).
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

        let listener = TcpListener::bind(config.server.socket_addr()).await?;
        let local_addr = listener.local_addr()?;

        Ok(BoundServer {
            listener,
            router,
            local_addr,
            tls: config.server.tls.clone(),
        })
    }
}

impl Default for ServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// A fully bound server ready to accept connections.
pub struct BoundServer {
    listener: TcpListener,
    router: axum::Router,
    local_addr: SocketAddr,
    /// TLS settings from the shared server config; `None` serves plain HTTP.
    tls: Option<rusty_photon_tls::config::TlsConfig>,
}

impl BoundServer {
    pub const fn listen_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Run the server until `shutdown` resolves.
    ///
    /// The runner ([`rusty_photon_service_lifecycle::ServiceRunner`])
    /// owns signal installation; this method just threads the shutdown
    /// future into the serve loop (TLS when `server.tls` is configured,
    /// plain `axum::serve` otherwise).
    ///
    /// # Errors
    ///
    /// Returns an error if the TLS material cannot be loaded or the
    /// serve loop fails.
    pub async fn start(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), std::io::Error> {
        let Self {
            listener,
            router,
            local_addr: _,
            tls,
        } = self;
        match tls {
            Some(ref tls_config) => {
                rusty_photon_tls::server::serve_tls(listener, router, tls_config, shutdown)
                    .await
                    .map_err(std::io::Error::other)
            }
            None => {
                axum::serve(listener, router)
                    .with_graceful_shutdown(shutdown)
                    .await
            }
        }
    }
}

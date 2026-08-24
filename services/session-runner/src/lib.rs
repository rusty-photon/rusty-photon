#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
//! `session-runner` — generic imaging-workflow orchestrator (an `rp`
//! orchestrator plugin).
//!
//! Design: `docs/services/session-runner.md`; delivery plan:
//! `docs/plans/archive/workflow-dsl.md`. This crate ships the expression layer
//! ([`expr`]), the document layer ([`document`]: model, validation layers
//! 1–2, parameter binding), the engine ([`engine`] + [`blackboard`],
//! triggers included), the SSE event client ([`events`]), and the service
//! wiring ([`mcp_client`], [`routes`], [`config`]) behind the two-phase
//! [`ServerBuilder`]. Still ahead in the plan: the resume BDD proof
//! (Phase D) and the deep-sky document (Phase E).

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

pub mod blackboard;
pub mod config;
pub mod document;
pub mod engine;
pub mod error;
pub mod events;
pub mod expr;
pub mod mcp_client;
pub mod routes;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use tracing::{debug, info};

use crate::config::Config;
use crate::error::{Result, SessionRunnerError};

/// Builder for the session-runner server (two-phase: build → start).
pub struct ServerBuilder {
    config: Option<Config>,
}

impl ServerBuilder {
    #[must_use]
    pub const fn new() -> Self {
        Self { config: None }
    }

    #[must_use]
    pub fn with_config(mut self, config: Config) -> Self {
        self.config = Some(config);
        self
    }

    /// Consume the builder and bind the configured listen address.
    ///
    /// # Errors
    ///
    /// Returns [`SessionRunnerError::Config`] if no configuration was
    /// supplied, or [`SessionRunnerError::Io`] if the listener cannot
    /// be bound or its address read.
    pub async fn build(self) -> Result<BoundServer> {
        let config = self.config.ok_or_else(|| {
            SessionRunnerError::Config(
                "ServerBuilder::build: configuration is required — call .with_config(...) first"
                    .to_owned(),
            )
        })?;
        let server = config.server.clone();
        let router = routes::build_router(Arc::new(config));

        // Layer HTTP Basic Auth when configured (server.auth).
        let router = match &server.auth {
            Some(auth) => {
                if server.tls.is_none() {
                    tracing::warn!(
                        "Authentication is enabled but TLS is not. Credentials will be \
                         transmitted in cleartext. Consider enabling TLS (see `doctor --fix`)."
                    );
                }
                rp_auth::layer(router, auth)
            }
            None => router,
        };

        let listener = tokio::net::TcpListener::bind(server.socket_addr()).await?;
        let local_addr = listener.local_addr()?;

        // This println is parsed by BDD tests to discover the bound port.
        // Console mode only: stdout is a dead handle under the Windows SCM,
        // and the only stdout consumer (bdd-infra's port parser) never runs
        // services with --service.
        if !rusty_photon_service_lifecycle::is_scm_service() {
            println!("Bound session-runner server bound_addr={local_addr}");
        }
        info!("session-runner service bound on {local_addr}");

        Ok(BoundServer {
            listener,
            router,
            local_addr,
            tls: server.tls,
        })
    }
}

impl Default for ServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// A fully bound session-runner server ready to accept connections.
pub struct BoundServer {
    listener: tokio::net::TcpListener,
    router: axum::Router,
    local_addr: SocketAddr,
    tls: Option<rusty_photon_tls::config::TlsConfig>,
}

impl BoundServer {
    pub const fn listen_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serve the bound listener until `shutdown` resolves.
    ///
    /// # Errors
    ///
    /// Returns [`SessionRunnerError::Server`] if the TLS material
    /// cannot be loaded or the serve loop fails.
    pub async fn start(self, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<()> {
        info!("session-runner service started on {}", self.local_addr);

        match self.tls {
            Some(ref tls) => {
                rusty_photon_tls::server::serve_tls(self.listener, self.router, tls, shutdown)
                    .await
                    .map_err(|e| SessionRunnerError::Server(e.to_string()))?;
            }
            None => axum::serve(self.listener, self.router)
                .with_graceful_shutdown(shutdown)
                .await
                .map_err(|e| SessionRunnerError::Server(e.to_string()))?,
        }

        debug!("session-runner service shut down");
        Ok(())
    }
}

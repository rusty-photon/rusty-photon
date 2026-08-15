#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
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

pub mod config;
pub mod doctor;
pub mod ephemeris;
pub mod error;
pub mod math;
pub mod mcp_client;
pub mod preview;
pub mod routes;
pub mod workflow;

use std::future::Future;
use std::net::SocketAddr;

use tracing::{debug, info};

use crate::config::PolarAlignConfig;
use crate::error::Result;

/// Builder for the polar-align server.
pub struct ServerBuilder {
    config: Option<PolarAlignConfig>,
}

impl ServerBuilder {
    #[must_use]
    pub const fn new() -> Self {
        Self { config: None }
    }

    #[must_use]
    pub fn with_config(mut self, config: PolarAlignConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub async fn build(self) -> Result<BoundServer> {
        let config = self.config.ok_or_else(|| {
            crate::error::PolarAlignError::Config(
                "ServerBuilder::build: config is required \u{2014} call .with_config(...) first"
                    .to_string(),
            )
        })?;
        let server = config.server.clone();

        let router = routes::build_router(config);

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
            println!("Bound polar-align server bound_addr={local_addr}");
        }
        info!("polar-align service bound on {}", local_addr);

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

/// A fully bound polar-align server ready to accept connections.
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

    pub async fn start(self, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<()> {
        info!("polar-align service started on {}", self.local_addr);

        match self.tls {
            Some(ref tls) => {
                rusty_photon_tls::server::serve_tls(self.listener, self.router, tls, shutdown)
                    .await
                    .map_err(|e| crate::error::PolarAlignError::Server(e.to_string()))?;
            }
            None => axum::serve(self.listener, self.router)
                .with_graceful_shutdown(shutdown)
                .await
                .map_err(|e| crate::error::PolarAlignError::Server(e.to_string()))?,
        }

        debug!("polar-align service shut down");
        Ok(())
    }
}

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
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
pub mod error;
pub mod mcp_client;
pub mod routes;
pub mod workflow;

use std::future::Future;
use std::net::SocketAddr;

use tracing::{debug, info};

use crate::config::FlatPlan;
use crate::error::Result;

/// Builder for the calibrator-flats server.
pub struct ServerBuilder {
    plan: Option<FlatPlan>,
}

impl ServerBuilder {
    #[must_use]
    pub const fn new() -> Self {
        Self { plan: None }
    }

    #[must_use]
    pub fn with_plan(mut self, plan: FlatPlan) -> Self {
        self.plan = Some(plan);
        self
    }

    /// Consume the builder and bind the configured listen address.
    ///
    /// # Errors
    ///
    /// Returns [`Config`](crate::error::CalibratorFlatsError::Config)
    /// if no flat plan was supplied, or
    /// [`Io`](crate::error::CalibratorFlatsError::Io) if the listener
    /// cannot be bound or its address read.
    pub async fn build(self) -> Result<BoundServer> {
        let plan = self.plan.ok_or_else(|| {
            crate::error::CalibratorFlatsError::Config(
                "ServerBuilder::build: flat plan is required \u{2014} call .with_plan(...) first"
                    .to_string(),
            )
        })?;
        let server = plan.server.clone();

        let router = routes::build_router(plan);

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
            println!("Bound calibrator-flats server bound_addr={local_addr}");
        }
        info!("calibrator-flats service bound on {}", local_addr);

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

/// A fully bound calibrator-flats server ready to accept connections.
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
    /// Returns [`Server`](crate::error::CalibratorFlatsError::Server)
    /// if the TLS material cannot be loaded or the serve loop fails.
    pub async fn start(self, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<()> {
        info!("calibrator-flats service started on {}", self.local_addr);

        match self.tls {
            Some(ref tls) => {
                rusty_photon_tls::server::serve_tls(self.listener, self.router, tls, shutdown)
                    .await
                    .map_err(|e| crate::error::CalibratorFlatsError::Server(e.to_string()))?;
            }
            None => axum::serve(self.listener, self.router)
                .with_graceful_shutdown(shutdown)
                .await
                .map_err(|e| crate::error::CalibratorFlatsError::Server(e.to_string()))?,
        }

        debug!("calibrator-flats service shut down");
        Ok(())
    }
}

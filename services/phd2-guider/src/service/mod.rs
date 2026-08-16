//! HTTP service mode (`phd2-guider serve`) — the rp-managed guider
//! service. See `docs/services/phd2-guider.md` § "HTTP Service Mode"
//! for the behavior contract and `docs/services/rp.md` § "Guider
//! Service" for how `rp` consumes it.

pub mod api;
pub mod error;
pub mod guider;

pub use error::{ErrorCode, ErrorResponse, ServiceError};
pub use guider::GuiderOps;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;

use crate::client::Phd2Client;
use crate::config::Config;

/// Two-phase server builder. `build()` binds the TCP listener (so the
/// bound port is known up-front), then `start()` serves. Mirrors
/// `services/plate-solver`'s `ServerBuilder` and avoids the port-TOCTOU
/// race noted in `docs/skills/development-workflow.md` § "Phase 4
/// Stabilization".
pub struct ServerBuilder {
    config: Option<Config>,
    client: Option<Arc<Phd2Client>>,
}

impl ServerBuilder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            config: None,
            client: None,
        }
    }

    #[must_use]
    pub fn with_config(mut self, config: Config) -> Self {
        self.config = Some(config);
        self
    }

    /// Override the PHD2 client (tests inject one wired to a mock
    /// connection factory; production builds one from `config.phd2`).
    #[must_use]
    pub fn with_client(mut self, client: Arc<Phd2Client>) -> Self {
        self.client = Some(client);
        self
    }

    pub async fn build(self) -> Result<BoundServer, std::io::Error> {
        let config = self.config.ok_or_else(|| {
            std::io::Error::other(
                "ServerBuilder::build: config is required \u{2014} call .with_config(...) first",
            )
        })?;

        let client = self
            .client
            .unwrap_or_else(|| Arc::new(Phd2Client::new(config.phd2.clone())));

        let ops = Arc::new(GuiderOps::new(
            client,
            config.settling.clone(),
            config.stop_timeout,
        ));

        // Bind before spawning any background task: a bind failure a
        // caller chooses to handle must not leak an event pump or a
        // PHD2 connect-retry loop.
        let listener = TcpListener::bind(config.server.socket_addr()).await?;
        let local_addr = listener.local_addr()?;

        ops.spawn_event_pump();
        // A failed initial connect is not fatal: PHD2 may start later.
        // The retry task establishes the first connection; the
        // client's auto-reconnect owns recovery after that.
        ops.spawn_connect_retry(config.phd2.reconnect.interval);

        let router = api::build_router(ops);

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

    /// Run the server until `shutdown` resolves. The runner
    /// ([`rusty_photon_service_lifecycle::ServiceRunner`]) owns signal
    /// installation; this method just threads the shutdown future into
    /// the serve loop (TLS when `server.tls` is configured, plain
    /// `axum::serve` otherwise).
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

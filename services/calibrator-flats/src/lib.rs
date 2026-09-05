#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
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

//! calibrator-flats: the flat-field tool provider
//! (docs/services/calibrator-flats.md). An MCP server `rp` aggregates
//! (`train_flats`, `take_flats`, `get_flat_training`) and an MCP client
//! of `rp` that drives the rig, with a redb store of flat timing per
//! train and filter in between.

pub mod config;
pub mod doctor;
pub mod error;
pub mod mcp_client;
pub mod routes;
pub mod store;
pub mod tools;
pub mod workflow;

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use tracing::{debug, info};

use crate::config::Config;
use crate::error::Result;

/// Builder for the calibrator-flats server.
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

    /// Consume the builder: open the store, assemble the router and
    /// bind the configured listen address.
    ///
    /// # Errors
    ///
    /// Returns [`Config`](crate::error::CalibratorFlatsError::Config) if
    /// no config was supplied or the store path cannot be resolved,
    /// [`Store`](crate::error::CalibratorFlatsError::Store) if the store
    /// cannot be opened, or [`Io`](crate::error::CalibratorFlatsError::Io)
    /// if the listener cannot be bound or its address read.
    pub async fn build(self) -> Result<BoundServer> {
        let config = self.config.ok_or_else(|| {
            crate::error::CalibratorFlatsError::Config(
                "ServerBuilder::build: config is required \u{2014} call .with_config(...) first"
                    .to_string(),
            )
        })?;
        let server = config.server.clone();

        let store_path = config.store_path()?;
        debug!(path = %store_path.display(), "opening the flat-timing store");
        let store = store::FlatStore::open(&store_path).await?;
        let handler = tools::FlatsHandler::new(Arc::new(config), Arc::new(store));
        debug!(tools = ?handler.tool_names(), "flats tools ready");

        let hostname = hostname::get().ok().and_then(|h| h.into_string().ok());
        let allowed_hosts = additional_allowed_hosts(
            server.bind_address,
            hostname.as_deref(),
            &interface_addrs(server.bind_address),
        );
        debug!(
            ?allowed_hosts,
            "MCP Host allowlist, in addition to the loopback defaults"
        );
        let router = routes::build_router(handler, allowed_hosts);

        // Layer HTTP Basic Auth when configured (server.auth): it guards
        // /mcp and /health alike.
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

/// The `Host` values `/mcp` accepts beyond rmcp's loopback defaults.
///
/// The machine's hostname, the explicit bind address, and — for a
/// wildcard bind, which names no reachable address of its own — every
/// non-loopback interface address. The same derivation `rp` uses for its
/// own `/mcp`, so `rp` can dial this provider by hostname or LAN address.
/// Entries carry no port (matching the name on any port).
#[must_use]
pub fn additional_allowed_hosts(
    bind_ip: IpAddr,
    hostname: Option<&str>,
    interface_addrs: &[IpAddr],
) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    if let Some(host) = hostname {
        hosts.push(host.to_string());
    }
    if !bind_ip.is_unspecified() {
        hosts.push(bind_ip.to_string());
    }
    hosts.extend(interface_addrs.iter().map(IpAddr::to_string));

    let mut seen = std::collections::HashSet::new();
    hosts.retain(|host| !host.is_empty() && seen.insert(host.clone()));
    hosts
}

/// Every non-loopback address a local interface answers on, for a
/// wildcard bind. Empty for an explicit bind and on enumeration failure,
/// which is logged and non-fatal.
fn interface_addrs(bind_ip: IpAddr) -> Vec<IpAddr> {
    if !bind_ip.is_unspecified() {
        return Vec::new();
    }
    match if_addrs::get_if_addrs() {
        Ok(interfaces) => interfaces
            .iter()
            .filter(|interface| !interface.is_loopback())
            .map(if_addrs::Interface::ip)
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "could not enumerate interface addresses for the MCP Host allowlist");
            Vec::new()
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn an_explicit_bind_adds_the_hostname_and_the_bind_address() {
        let hosts = additional_allowed_hosts(ip("192.168.1.10"), Some("rig"), &[]);
        assert_eq!(hosts, ["rig", "192.168.1.10"]);
    }

    #[test]
    fn a_wildcard_bind_adds_the_interface_addresses_instead_deduplicated() {
        let hosts = additional_allowed_hosts(
            ip("0.0.0.0"),
            Some("rig"),
            &[ip("192.168.1.10"), ip("10.0.0.5"), ip("192.168.1.10")],
        );
        assert_eq!(hosts, ["rig", "192.168.1.10", "10.0.0.5"]);
    }

    #[test]
    fn an_empty_hostname_is_dropped() {
        let hosts = additional_allowed_hosts(ip("0.0.0.0"), Some(""), &[]);
        assert!(hosts.is_empty());
    }

    #[test]
    fn interface_addrs_is_empty_for_an_explicit_bind() {
        assert!(interface_addrs(ip("127.0.0.1")).is_empty());
    }
}

//! The Alpaca UDP discovery responder, wired to the drivers' optional
//! `server.discovery_port` config field.
//!
//! Every service serves HTTP itself through rusty-photon-tls
//! ([`ascom_alpaca::Server::into_service`] for the TLS/auth layering) rather
//! than `ascom_alpaca::Server::start()` — and only the latter spawns the
//! crate's own discovery responder. `into_service`'s documented contract is
//! that "the caller is responsible for starting the Alpaca discovery server
//! separately if needed"; this module is that caller-owned responder. (The
//! responder was lost when PR #63 introduced the rusty-photon-tls serving path.)
//!
//! Discovery is **off unless the config opts in**: rusty-photon runs up to
//! 14 Alpaca servers on one host, and a default-on responder in every one
//! would collide on the shared discovery port (with `SO_REUSEPORT`, a
//! unicast query is answered by an arbitrary one of them). An absent /
//! `null` `discovery_port` — the default — means no responder; an explicit
//! port (normally 32227, the Alpaca standard) opts in, for hosts running a
//! single driver.

use ascom_alpaca::discovery::{BoundDiscoveryServer, DiscoveryServer};
use std::net::SocketAddr;
use tracing::debug;

/// Bind the discovery responder advertising the Alpaca server at
/// `bound_addr`, if `discovery_port` opts in; `Ok(None)` when discovery is
/// disabled.
///
/// Binding happens at service startup so that a taken discovery port fails
/// startup loudly instead of surfacing as clients silently not finding the
/// device.
///
/// # Errors
///
/// Returns the bind error when the discovery port cannot be bound (e.g.
/// already taken).
pub async fn bind(
    bound_addr: SocketAddr,
    discovery_port: Option<u16>,
) -> Result<Option<BoundDiscoveryServer>, Box<dyn std::error::Error + Send + Sync>> {
    let Some(port) = discovery_port else {
        debug!("Alpaca discovery disabled (no discovery_port in config)");
        return Ok(None);
    };
    let mut server = DiscoveryServer::for_alpaca_server_at(bound_addr);
    server.listen_addr.set_port(port);
    let bound = server.bind().await?;
    debug!(listen_addr = %bound.listen_addr(), "Alpaca discovery responder bound");
    Ok(Some(bound))
}

/// Run the serve future with the responder from [`bind`] answering
/// alongside it; resolves with the serve future's output.
///
/// The responder is deliberately NOT `tokio::spawn`ed off: it lives inside
/// a `select!` with the serve future, so when serving ends (shutdown or
/// SIGHUP reload) the responder is dropped and its socket closed — a
/// reload's rebuilt server can immediately rebind the discovery port. A
/// detached task would hold the port and break every reload.
pub async fn serve_with<F: std::future::Future>(
    bound: Option<BoundDiscoveryServer>,
    serve: F,
) -> F::Output {
    match bound {
        None => serve.await,
        Some(responder) => {
            tokio::select! {
                out = serve => out,
                never = responder.start() => match never {},
            }
        }
    }
}

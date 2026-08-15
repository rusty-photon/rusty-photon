//! UDP/WiFi (mount in AP mode, port 11880) [`TransportFactory`].
//!
//! Binds a local socket — **mandatory** in the configured bind subnet
//! because the mount silently drops replies that can't be routed back
//! — `connect()`s it to the peer, and wraps it in a
//! [`UdpFrameTransport`]. Datagram boundaries become frame boundaries
//! by construction; framing strictness (exactly one well-formed
//! `:cmd<axis><payload>\r` per packet) is preserved.

use std::io;
use std::net::SocketAddr;

use async_trait::async_trait;
use rusty_photon_shared_transport::{
    FrameTransport, TransportError, TransportFactory, UdpFrameTransport,
};
use tokio::net::UdpSocket;
use tracing::debug;

use crate::config::UdpConfig;

/// Maximum size of a single UDP frame.
///
/// The Sky-Watcher protocol's longest documented reply payload is
/// 8 bytes; 256 bytes is a generous bound that still catches a
/// runaway peer's malformed jumbo frame.
const MAX_FRAME_SIZE: usize = 256;

/// Real-hardware factory for the Sky-Watcher UDP transport.
#[derive(Debug, Clone)]
pub struct UdpTransportFactory {
    config: UdpConfig,
}

impl UdpTransportFactory {
    /// Capture the per-call configuration (peer address, bind address,
    /// timeout) at factory-construction time so [`TransportFactory::open`]
    /// can be retried by the shared-transport core without rethreading
    /// parameters.
    #[must_use]
    pub const fn new(config: UdpConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl TransportFactory for UdpTransportFactory {
    async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError> {
        let bind_addr = SocketAddr::new(self.config.bind_address, 0);
        let mount_addr = SocketAddr::new(self.config.address, self.config.port);
        debug!(
            bind = %bind_addr,
            mount = %mount_addr,
            timeout = ?self.config.command_timeout,
            "opening Sky-Watcher UDP transport"
        );

        // `io::Error::new(e.kind(), ...)` instead of
        // `io::Error::other(format!(...))` so the original
        // `io::ErrorKind` (e.g. `AddrNotAvailable`, `PermissionDenied`,
        // `AddrInUse`) survives downstream classification while the
        // bind / connect address stays in the surfaced message. See
        // Copilot review on PR #285.
        let socket = UdpSocket::bind(bind_addr).await.map_err(|e| {
            TransportError::Open(io::Error::new(
                e.kind(),
                format!("UDP bind {bind_addr}: {e}"),
            ))
        })?;
        socket.connect(mount_addr).await.map_err(|e| {
            TransportError::Open(io::Error::new(
                e.kind(),
                format!("UDP connect {mount_addr}: {e}"),
            ))
        })?;

        let transport = UdpFrameTransport::new(socket, MAX_FRAME_SIZE)
            .with_read_timeout(self.config.command_timeout)
            .with_write_timeout(self.config.command_timeout);
        Ok(Box::new(transport))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::Duration;
    use tokio::net::UdpSocket;

    fn cfg_for(server_addr: SocketAddr, timeout: Duration) -> UdpConfig {
        UdpConfig {
            address: server_addr.ip(),
            port: server_addr.port(),
            bind_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            command_timeout: timeout,
            polling_interval: Duration::from_millis(200),
        }
    }

    /// Spawn a localhost UDP echo server that returns canned replies
    /// for any incoming datagram. Replicates the legacy
    /// `transport::udp::tests::spawn_echo_server` so the factory's
    /// `open()` body has the same coverage it had before Phase E.
    async fn spawn_echo_server(reply: Vec<u8>) -> SocketAddr {
        let server = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            loop {
                let Ok((_n, peer)) = server.recv_from(&mut buf).await else {
                    return;
                };
                let _ = server.send_to(&reply, peer).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn factory_open_against_localhost_echo_round_trips_one_frame() {
        let server_addr = spawn_echo_server(b"=000080\r".to_vec()).await;
        let factory = UdpTransportFactory::new(cfg_for(server_addr, Duration::from_secs(1)));
        let mut transport = factory.open().await.unwrap();

        transport.send_frame(b":j1\r").await.unwrap();
        let mut reply = Vec::new();
        transport.recv_frame(&mut reply).await.unwrap();
        assert_eq!(reply, b"=000080\r");
    }

    #[tokio::test]
    async fn factory_open_returns_raw_datagram_with_trailing_newline() {
        // The `UdpFrameTransport` returns the whole datagram verbatim
        // — including any trailing `\n` the firmware appends. The
        // SkywatcherCodec's `normalize_response_frame` strips it
        // downstream; here we just verify the transport doesn't
        // silently swallow bytes.
        let server_addr = spawn_echo_server(b"=000080\r\n".to_vec()).await;
        let factory = UdpTransportFactory::new(cfg_for(server_addr, Duration::from_secs(1)));
        let mut transport = factory.open().await.unwrap();

        transport.send_frame(b":j1\r").await.unwrap();
        let mut reply = Vec::new();
        transport.recv_frame(&mut reply).await.unwrap();
        assert_eq!(reply, b"=000080\r\n");
    }

    #[tokio::test]
    async fn factory_open_recv_times_out_when_server_is_silent() {
        // Bind a server but don't reply.
        let server = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let server_addr = server.local_addr().unwrap();
        // Keep the socket alive for the duration of the test so the
        // OS doesn't immediately reject our writes; reads to it have
        // no responder.
        let _keepalive = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(server);
        });
        let factory = UdpTransportFactory::new(cfg_for(server_addr, Duration::from_millis(80)));
        let mut transport = factory.open().await.unwrap();

        transport.send_frame(b":j1\r").await.unwrap();
        let mut reply = Vec::new();
        let err = transport.recv_frame(&mut reply).await.unwrap_err();
        assert!(matches!(err, TransportError::Timeout(_)), "got {err:?}");
    }

    // The previous `factory_open_bind_fails_on_non_local_subnet` test
    // (bind to 198.51.100.1, TEST-NET-2) was dropped: the
    // bind-failure-surfaces-as-`TransportError::Open` contract is
    // already exercised by the serial side's equivalent
    // (`factory_open_nonexistent_port_returns_open_error`), and using
    // a "guaranteed not on any local interface" address is host-dependent
    // — it accepts under `net.ipv4.ip_nonlocal_bind=1`, `IP_FREEBIND`,
    // and some container test runners. The mapping itself is a single
    // `.map_err(...)` line above (`UdpSocket::bind` returns `io::Error`;
    // we wrap it in `TransportError::Open` while preserving the kind),
    // so a unit test of the mapping doesn't need a real bind to fire.
    // See PR #285 review.
}

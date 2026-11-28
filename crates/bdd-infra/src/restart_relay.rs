//! The port a client keeps dialing across a service process's restart.
//!
//! A scenario that stops a service and starts it again, while a client
//! keeps the address it was configured with, needs that port held through
//! the gap. Released and rebound, it is taken by a concurrent test binary
//! often enough to fail a scenario on a healthy tree: the restarted service
//! fails its bind with `AddrInUse`. So the test binds the client's port
//! itself, once, and never releases it; the service binds port 0 on every
//! start, and the relay forwards to whichever port it announced
//! (docs/skills/testing.md §5.1). An in-process stub holds its own socket
//! instead (`rp_harness/held_port.rs`).

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};
use tracing::debug;

/// What [`RestartRelay::upstream`] holds while the service is stopped;
/// port 0 is never a port a service announced.
const STOPPED: u16 = 0;

/// A loopback port the test holds for a whole scenario.
///
/// Each connection to it is forwarded to the service's current port; while
/// the service is stopped, each is accepted and dropped unanswered, which a
/// client sees as a service that is down. Dropping the relay ends every
/// connection.
#[derive(Debug)]
pub struct RestartRelay {
    port: u16,
    /// The service's own port, [`STOPPED`] while it is stopped.
    upstream: Arc<AtomicU16>,
    task: JoinHandle<()>,
}

impl RestartRelay {
    /// Bind the relay on an OS-assigned loopback port, dropping every
    /// connection until [`Self::forward_to`] names the service's port.
    ///
    /// # Panics
    ///
    /// Panics if no loopback port can be bound.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind the restart relay");
        let port = listener
            .local_addr()
            .expect("the restart relay has no local addr")
            .port();
        let upstream = Arc::new(AtomicU16::new(STOPPED));
        let task = tokio::spawn(relay(listener, Arc::clone(&upstream)));
        debug!(port, "restart relay listening");
        Self {
            port,
            upstream,
            task,
        }
    }

    /// The port clients dial; it stays the same for the relay's life.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// `http://127.0.0.1:<port>`, the base URL clients are configured with.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Forward each connection to the service's `port` from now on.
    pub fn forward_to(&self, port: u16) {
        debug!(relay = self.port, port, "restart relay forwards");
        self.upstream.store(port, Ordering::SeqCst);
    }

    /// Drop each connection unanswered from now on. Call it before the
    /// service stops, so no connection opened after the call reaches the
    /// port the service frees, which another process may take.
    pub fn refuse(&self) {
        debug!(relay = self.port, "restart relay refuses");
        self.upstream.store(STOPPED, Ordering::SeqCst);
    }
}

impl Drop for RestartRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The accept loop: forward each connection to `upstream`'s port, or drop
/// it while that is [`STOPPED`].
async fn relay(listener: TcpListener, upstream: Arc<AtomicU16>) {
    // Aborting the loop drops the set, which ends every forwarded
    // connection.
    let mut connections = JoinSet::new();
    loop {
        while connections.try_join_next().is_some() {}
        let mut inbound = match listener.accept().await {
            Ok((inbound, _)) => inbound,
            Err(e) => {
                // A persistent accept error (descriptor exhaustion) must
                // not become a busy loop.
                debug!(error = %e, "restart relay accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let port = upstream.load(Ordering::SeqCst);
        if port == STOPPED {
            continue;
        }
        connections.spawn(async move {
            match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(mut outbound) => {
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }
                Err(e) => debug!(port, error = %e, "restart relay cannot reach the service"),
            }
        });
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A service stand-in on an OS-assigned port: it answers each
    /// connection with `reply` and closes it.
    async fn service(reply: &'static [u8]) -> (u16, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                stream.write_all(reply).await.unwrap();
            }
        });
        (port, task)
    }

    /// What a client dialing the relay reads before the connection ends.
    async fn read_through(relay: &RestartRelay) -> Vec<u8> {
        let mut stream = TcpStream::connect(("127.0.0.1", relay.port()))
            .await
            .unwrap();
        let mut answer = Vec::new();
        // A dropped connection may end in a reset rather than an EOF;
        // either way the client read nothing.
        let _ = stream.read_to_end(&mut answer).await;
        answer
    }

    #[tokio::test]
    async fn a_relay_drops_every_connection_until_it_forwards() {
        let relay = RestartRelay::start().await;
        assert_eq!(read_through(&relay).await, b"");
    }

    #[tokio::test]
    async fn a_relay_forwards_to_the_port_it_was_given() {
        let relay = RestartRelay::start().await;
        let (port, _service) = service(b"first").await;
        relay.forward_to(port);
        assert_eq!(read_through(&relay).await, b"first");
    }

    #[tokio::test]
    async fn a_refusing_relay_reaches_no_service() {
        let relay = RestartRelay::start().await;
        let (port, _service) = service(b"first").await;
        relay.forward_to(port);
        relay.refuse();
        assert_eq!(read_through(&relay).await, b"");
    }

    #[tokio::test]
    async fn a_restarted_service_on_a_new_port_answers_on_the_relay_s_port() {
        let relay = RestartRelay::start().await;
        let (first, first_service) = service(b"first").await;
        relay.forward_to(first);
        assert_eq!(read_through(&relay).await, b"first");
        relay.refuse();
        first_service.abort();
        let (second, _service) = service(b"second").await;
        relay.forward_to(second);
        assert_eq!(read_through(&relay).await, b"second");
    }
}

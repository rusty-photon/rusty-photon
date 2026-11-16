//! The one loopback socket a restartable stub listens on for its whole
//! life, through every stop and restart.
//!
//! A stopped stub keeps accepting connections and drops each one
//! unanswered, which a client sees as a dead service, while the port can
//! never be handed to another process in the meantime. Test shards run
//! concurrently and bind OS-assigned ports of their own; a port released
//! between a stop and a restart is taken by another shard's server often
//! enough to fail a scenario on a healthy tree.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Bind a loopback listener on an OS-assigned port — once per stub;
/// the socket is never released and rebound.
pub(super) fn bind_loopback() -> std::io::Result<TcpListener> {
    let socket = tokio::net::TcpSocket::new_v4()?;
    socket.bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
    socket.listen(64)
}

/// `axum::serve` wants to own its listener; this hands it a shared
/// handle instead, so the socket outlives the server and the refuser
/// can take it over without the port ever being released.
pub(super) struct SharedListener(pub(super) Arc<TcpListener>);

impl axum::serve::Listener for SharedListener {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.0.accept().await {
                Ok(pair) => return pair,
                // A transient accept error (a reset before accept, a
                // descriptor shortage): back off briefly, as axum's own
                // listener does, rather than spin.
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.0.local_addr()
    }
}

/// Stand in for a stopped service: accept every connection on
/// `listener` and drop it unanswered, keeping the port bound, until the
/// returned sender fires.
pub(super) fn refuse(listener: Arc<TcpListener>) -> (oneshot::Sender<()>, JoinHandle<()>) {
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => drop(stream),
                    // Same backoff as the serving listener: a
                    // persistent accept error (descriptor
                    // exhaustion) must not become a busy loop.
                    Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                }
            }
        }
    });
    (shutdown_tx, task)
}

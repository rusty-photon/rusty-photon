//! Tokio-serial-backed [`TransportFactory`] for the Sky-Watcher USB-CDC
//! transport.
//!
//! Opens the configured serial port at the configured baud rate and
//! wraps it in a [`SerialFrameTransport`] with `\r` as the frame
//! terminator (every Sky-Watcher reply ends with `\r`).
//!
//! Maps the per-service [`UsbConfig`] onto the shared crate's
//! [`TransportFactory`] surface so the shared-transport core (refcount,
//! handshake, while-open task) can drive the lifecycle.

use std::time::Duration;

use async_trait::async_trait;
use rusty_photon_shared_transport::{
    open_serial_port, FrameTransport, SerialFrameTransport, TransportError, TransportFactory,
};
use tracing::debug;

use crate::config::UsbConfig;

/// Maximum size of a single Sky-Watcher response frame.
///
/// Replies are at most `=<6 hex chars>\r` (8 bytes) on every documented
/// command; 64 bytes gives the firmware ample headroom and bounds a
/// misbehaving peer that streams without a `\r`.
const MAX_FRAME_SIZE: usize = 64;

/// Real-hardware factory for the Sky-Watcher serial transport.
#[derive(Debug, Clone)]
pub struct SerialTransportFactory {
    port: String,
    baud_rate: u32,
    command_timeout: Duration,
}

impl SerialTransportFactory {
    /// Construct a factory from a [`UsbConfig`]. The factory captures
    /// the port path, baud rate, and timeout once at startup so
    /// [`TransportFactory::open`] can be retried without rethreading
    /// configuration.
    #[must_use]
    pub fn new(config: UsbConfig) -> Self {
        Self {
            port: config.port,
            baud_rate: config.baud_rate,
            command_timeout: config.command_timeout,
        }
    }
}

#[async_trait]
impl TransportFactory for SerialTransportFactory {
    async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError> {
        debug!(
            port = %self.port,
            baud = self.baud_rate,
            timeout = ?self.command_timeout,
            "opening Sky-Watcher serial transport"
        );

        // The shared opener owns the builder settings, the error
        // mapping, and the retry that rides out a handle the OS has not
        // finished releasing — see `open_serial_port`.
        let stream = open_serial_port(&self.port, self.baud_rate).await?;

        let transport = SerialFrameTransport::new(stream, b'\r', MAX_FRAME_SIZE)
            .with_read_timeout(self.command_timeout)
            .with_write_timeout(self.command_timeout);
        Ok(Box::new(transport))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn factory_open_nonexistent_port_returns_open_error() {
        use std::error::Error;
        let factory = SerialTransportFactory::new(UsbConfig {
            port: "/dev/this-port-does-not-exist-xyzzy".into(),
            ..UsbConfig::default()
        });
        match factory.open().await {
            Err(TransportError::Open(io_err)) => {
                // `io::Error::other(e)` (vs `io::Error::other(e.to_string())`)
                // preserves the original `tokio_serial::Error` as the
                // io::Error's source, so log/debug output traversing
                // `Error::source()` recovers the underlying cause. The
                // assertion catches regressions back to the
                // stringified shape, which loses that source.
                assert!(
                    io_err.source().is_some() || io_err.get_ref().is_some(),
                    "expected the underlying tokio_serial::Error to be preserved as source"
                );
            }
            Err(other) => panic!("expected TransportError::Open, got {other:?}"),
            Ok(_) => panic!("expected error opening nonexistent port"),
        }
    }
}

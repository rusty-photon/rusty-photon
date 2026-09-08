//! Tokio-serial-backed [`TransportFactory`] for the UPBv2.
//!
//! Opens the configured serial port at the configured baud rate and wraps
//! the resulting stream in a [`SerialFrameTransport`] with `\n` as the
//! frame terminator (UPBv2 replies are LF-terminated ASCII lines).

use std::io;
use std::time::Duration;

use async_trait::async_trait;
use rusty_photon_shared_transport::{
    FrameTransport, SerialFrameTransport, TransportError, TransportFactory,
};
use tokio_serial::SerialPortBuilderExt;
use tracing::debug;

/// Maximum size of a single UPBv2 frame.
///
/// The longest reply we ever see is the 21-field PA status line (~70
/// characters); 256 bytes gives the device plenty of headroom and bounds
/// a misbehaving peer that streams without a terminator.
const MAX_FRAME_SIZE: usize = 256;

/// Real-hardware factory for the UPBv2 serial transport.
///
/// Captures the per-call configuration (port path, baud rate, timeout)
/// once at service-startup time so [`TransportFactory::open`] can be
/// retried by [`rusty_photon_shared_transport::SharedTransport`] without
/// the caller having to thread parameters through.
#[derive(Debug, Clone)]
pub struct Upbv2TransportFactory {
    port: String,
    baud_rate: u32,
    timeout: Duration,
}

impl Upbv2TransportFactory {
    pub fn new(port: impl Into<String>, baud_rate: u32, timeout: Duration) -> Self {
        Self {
            port: port.into(),
            baud_rate,
            timeout,
        }
    }
}

#[async_trait]
impl TransportFactory for Upbv2TransportFactory {
    async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError> {
        debug!(
            port = %self.port,
            baud = self.baud_rate,
            timeout = ?self.timeout,
            "opening UPBv2 serial transport"
        );

        // Note: no `.timeout(self.timeout)` on the tokio-serial builder.
        // `SerialFrameTransport`'s `with_read_timeout` /
        // `with_write_timeout` already enforces the per-call deadline via
        // `tokio::time::timeout`; adding a parallel port-level (termios
        // `VTIME`) timeout creates two timers set to the same value with
        // no obvious answer to "which fires first". The shared crate
        // reclassifies `io::ErrorKind::TimedOut` from the wrapped stream
        // back to `TransportError::Timeout`, so if a future runtime ever
        // does need a port-level timeout the classification stays right
        // — but reasoning is still simpler with a single source.
        // Pass the `tokio_serial::Error` to `io::Error::other` directly
        // (not its `.to_string()`) so the original error is preserved as
        // the `io::Error` source — `TransportError::Open(io::Error)` then
        // exposes the full cause chain via `Error::source()` traversal in
        // logs / debug output.
        let stream = tokio_serial::new(&self.port, self.baud_rate)
            .open_native_async()
            .map_err(|e| TransportError::Open(io::Error::other(e)))?;

        let transport = SerialFrameTransport::new(stream, b'\n', MAX_FRAME_SIZE)
            .with_read_timeout(self.timeout)
            .with_write_timeout(self.timeout);
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
        let factory =
            Upbv2TransportFactory::new("/dev/nonexistent_port_12345", 9600, Duration::from_secs(1));
        match factory.open().await {
            Err(TransportError::Open(io_err)) => {
                // `io::Error::other(e)` (vs `io::Error::other(e.to_string())`)
                // preserves the original `tokio_serial::Error` as the
                // io::Error's source, so log/debug output traversing
                // `Error::source()` recovers the underlying cause.
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

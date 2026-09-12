//! Tokio-serial-backed [`TransportFactory`] for the QHY Q-Focuser.
//!
//! Opens the configured serial port at the configured baud rate and wraps
//! the resulting stream in a [`SerialFrameTransport`] with `b'}'` as the
//! frame terminator (Q-Focuser responses are flat JSON objects terminated
//! by the closing brace; the codec sees the brace as the last byte of
//! each frame).

use std::time::Duration;

use async_trait::async_trait;
use rusty_photon_shared_transport::{
    open_serial_port, FrameTransport, SerialFrameTransport, TransportError, TransportFactory,
};
use tracing::debug;

/// Maximum size of a single Q-Focuser frame.
///
/// The longest reply is the temperature/voltage line which fits in about
/// 60 bytes; 256 bytes gives the device plenty of headroom and bounds a
/// misbehaving peer that streams without a terminator.
const MAX_FRAME_SIZE: usize = 256;

/// Real-hardware factory for the Q-Focuser serial transport.
///
/// Captures the per-call configuration (port path, baud rate, timeout)
/// once at service-startup time so [`TransportFactory::open`] can be
/// retried by [`rusty_photon_shared_transport::SharedTransport`] without
/// the caller having to thread parameters through.
#[derive(Debug, Clone)]
pub struct QhyTransportFactory {
    port: String,
    baud_rate: u32,
    timeout: Duration,
}

impl QhyTransportFactory {
    pub fn new(port: impl Into<String>, baud_rate: u32, timeout: Duration) -> Self {
        Self {
            port: port.into(),
            baud_rate,
            timeout,
        }
    }
}

#[async_trait]
impl TransportFactory for QhyTransportFactory {
    async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError> {
        debug!(
            port = %self.port,
            baud = self.baud_rate,
            timeout = ?self.timeout,
            "opening Q-Focuser serial transport"
        );

        // The shared opener owns the builder settings, the error
        // mapping, and the retry that rides out a handle the OS has not
        // finished releasing — see `open_serial_port`.
        let stream = open_serial_port(&self.port, self.baud_rate).await?;

        let transport = SerialFrameTransport::new(stream, b'}', MAX_FRAME_SIZE)
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
    #[cfg_attr(miri, ignore)] // tokio-serial uses unsupported syscall flags under Miri
    async fn factory_open_nonexistent_port_returns_open_error() {
        let factory =
            QhyTransportFactory::new("/dev/nonexistent_port_12345", 9600, Duration::from_secs(1));
        match factory.open().await {
            Err(TransportError::Open(_)) => {
                // Only the variant is this factory's to pin: the
                // opening — and keeping the underlying
                // `tokio_serial::Error` rather than its text — belongs
                // to `open_serial_port`, and is asserted there where
                // the type can be named.
            }
            Err(other) => panic!("expected TransportError::Open, got {other:?}"),
            Ok(_) => panic!("expected error opening nonexistent port"),
        }
    }
}

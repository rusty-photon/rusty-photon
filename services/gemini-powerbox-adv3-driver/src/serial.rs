//! Tokio-serial-backed [`TransportFactory`] for the Advanced 3.
//!
//! Opens the configured serial port at the hub's one baud rate and wraps
//! the resulting stream in a [`SerialFrameTransport`] with `#` as the frame
//! terminator: the hub's replies are `*X…#` with no line ending.

use std::time::Duration;

use async_trait::async_trait;
use rusty_photon_shared_transport::{
    open_serial_port, FrameTransport, SerialFrameTransport, TransportError, TransportFactory,
};
use tracing::debug;

/// The hub speaks 19200 baud and nothing else, so the configuration carries
/// no baud rate and the factory pins it.
pub const BAUD_RATE: u32 = 19200;

/// Every reply ends in `#`, and no reply carries one inside.
pub const FRAME_TERMINATOR: u8 = b'#';

/// Maximum size of a single frame.
///
/// The status frame is 81 bytes; 256 gives the firmware headroom and bounds
/// a peer that streams without a terminator.
const MAX_FRAME_SIZE: usize = 256;

/// Real-hardware factory for the hub's serial transport.
///
/// Captures the per-call configuration (port path, timeout) once at
/// service-startup time so [`TransportFactory::open`] can be retried by
/// [`rusty_photon_shared_transport::SharedTransport`] without the caller
/// having to thread parameters through.
#[derive(Debug, Clone)]
pub struct GeminiTransportFactory {
    port: String,
    timeout: Duration,
}

impl GeminiTransportFactory {
    pub fn new(port: impl Into<String>, timeout: Duration) -> Self {
        Self {
            port: port.into(),
            timeout,
        }
    }
}

#[async_trait]
impl TransportFactory for GeminiTransportFactory {
    async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError> {
        debug!(
            port = %self.port,
            baud = BAUD_RATE,
            timeout = ?self.timeout,
            "opening Gemini hub serial transport"
        );

        // The shared opener owns the builder settings, the error mapping,
        // and the retry that rides out a handle the OS has not finished
        // releasing — see `open_serial_port`. It raises DTR and RTS, which
        // the dev box's unit was measured not to mind.
        let stream = open_serial_port(&self.port, BAUD_RATE).await?;

        let transport = SerialFrameTransport::new(stream, FRAME_TERMINATOR, MAX_FRAME_SIZE)
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
        let factory =
            GeminiTransportFactory::new("/dev/nonexistent_port_12345", Duration::from_secs(1));
        match factory.open().await {
            Err(TransportError::Open(_)) => {}
            Err(other) => panic!("expected TransportError::Open, got {other:?}"),
            Ok(_) => panic!("expected error opening nonexistent port"),
        }
    }

    #[test]
    fn the_baud_rate_is_the_hubs_only_one() {
        assert_eq!(BAUD_RATE, 19200);
        assert_eq!(FRAME_TERMINATOR, b'#');
    }
}

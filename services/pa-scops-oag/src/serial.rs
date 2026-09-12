//! Tokio-serial-backed [`TransportFactory`] for the Pegasus Scops OAG.
//!
//! Opens the configured serial port at the configured baud rate (default 19200
//! — see [`crate::config::SerialConfig`]). Framing is 8N1 (tokio-serial's
//! default), with `\n` as the command-line terminator and `\r\n` on responses.
//! The stream is wrapped in a [`SerialFrameTransport`] with `b'\n'` framing.

use std::time::Duration;

use async_trait::async_trait;
use rusty_photon_shared_transport::{
    open_serial_port, FrameTransport, SerialFrameTransport, TransportError, TransportFactory,
};
use tracing::debug;

/// Maximum size of a single Scops frame.
///
/// The longest reply is the `A` status line (`OK_SCOPS:` plus nine
/// colon-delimited fields, well under 60 characters); 256 bytes gives ample
/// headroom and bounds a misbehaving peer that streams without a terminator.
const MAX_FRAME_SIZE: usize = 256;

/// Real-hardware factory for the Scops OAG serial transport.
#[derive(Debug, Clone)]
pub struct ScopsTransportFactory {
    port: String,
    baud_rate: u32,
    timeout: Duration,
}

impl ScopsTransportFactory {
    pub fn new(port: impl Into<String>, baud_rate: u32, timeout: Duration) -> Self {
        Self {
            port: port.into(),
            baud_rate,
            timeout,
        }
    }

    /// Build a factory from the service [`SerialConfig`](crate::config::SerialConfig).
    #[must_use]
    pub fn from_config(config: &crate::config::SerialConfig) -> Self {
        Self::new(config.port.clone(), config.baud_rate, config.timeout)
    }
}

#[async_trait]
impl TransportFactory for ScopsTransportFactory {
    /// Coverage off: the success path requires a real serial device. The
    /// open-failure path is covered by
    /// `factory_open_nonexistent_port_returns_open_error` below.
    #[cfg_attr(coverage_nightly, coverage(off))]
    async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError> {
        debug!(
            port = %self.port,
            baud = self.baud_rate,
            timeout = ?self.timeout,
            "opening Scops OAG serial transport"
        );

        // The shared opener owns the builder settings, the error
        // mapping, and the retry that rides out a handle the OS has not
        // finished releasing — see `open_serial_port`.
        let stream = open_serial_port(&self.port, self.baud_rate).await?;

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
    #[cfg_attr(miri, ignore)]
    async fn factory_open_nonexistent_port_returns_open_error() {
        let factory = ScopsTransportFactory::new(
            "/dev/nonexistent_scops_12345",
            19200,
            Duration::from_secs(1),
        );
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

    #[test]
    fn from_config_propagates_fields() {
        let cfg = crate::config::SerialConfig {
            port: "/dev/ttyTEST".to_string(),
            baud_rate: 19200,
            polling_interval: Duration::from_millis(500),
            timeout: Duration::from_secs(3),
        };
        let factory = ScopsTransportFactory::from_config(&cfg);
        assert_eq!(factory.port, "/dev/ttyTEST");
        assert_eq!(factory.baud_rate, 19200);
        assert_eq!(factory.timeout, Duration::from_secs(3));
    }
}

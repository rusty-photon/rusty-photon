//! [`TransportFactory`] for the FP2's USB-CDC serial port.
//!
//! Wraps a fresh `tokio_serial::SerialStream` in a
//! [`SerialFrameTransport`] configured to read until the FP2's `)`
//! response terminator. The factory captures port path / baud rate /
//! timeout at startup; each `open()` call corresponds to one
//! `SharedTransport` 0→1 connect transition.

use std::time::Duration;

use async_trait::async_trait;
use rusty_photon_shared_transport::{
    FrameTransport, SerialFrameTransport, TransportError, TransportFactory,
};
use tokio_serial::{SerialPort, SerialPortBuilderExt};
use tracing::debug;

/// Upper bound on a single FP2 response frame. The firmware identification
/// string is the longest real response and sits well under 64 bytes; a 256
/// cap is generous and still ensures runaway peers can't grow the read
/// buffer unboundedly (the cap is enforced by `SerialFrameTransport`).
const MAX_FRAME_SIZE: usize = 256;

/// Factory that opens the FP2 over `tokio-serial`.
#[derive(Debug, Clone)]
pub struct Fp2SerialTransportFactory {
    port: String,
    baud_rate: u32,
    timeout: Duration,
}

impl Fp2SerialTransportFactory {
    pub fn new(port: impl Into<String>, baud_rate: u32, timeout: Duration) -> Self {
        Self {
            port: port.into(),
            baud_rate,
            timeout,
        }
    }
}

#[async_trait]
impl TransportFactory for Fp2SerialTransportFactory {
    async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError> {
        debug!(
            "Opening FP2 serial port {} at {} baud (timeout {:?})",
            self.port, self.baud_rate, self.timeout
        );

        // No `.timeout(self.timeout)` on the tokio-serial builder.
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
        let mut stream = tokio_serial::new(&self.port, self.baud_rate)
            .open_native_async()
            .map_err(|e| TransportError::Open(std::io::Error::other(e)))?;

        // The FP2's RP2040 USB-CDC firmware transmits only while the host
        // holds DTR high. Linux raises DTR as a side effect of opening the
        // tty, so the requirement is invisible there; Windows leaves the
        // line low and every command then times out unanswered — the
        // startup handshake included, which crash-loops the service. The
        // builder's `dtr_on_open` cannot carry this: on Windows the async
        // open probes the port through a synchronous handle (where that
        // flag is applied), closes it — dropping DTR with it — and reopens
        // the path in overlapped mode, re-applying only the line settings.
        // Asserting DTR on the handle actually kept is what reaches the
        // device, and it is a no-op where the kernel already raised it.
        stream
            .write_data_terminal_ready(true)
            .map_err(|e| TransportError::Open(std::io::Error::other(e)))?;

        debug!("FP2 serial port {} opened", self.port);

        let transport = SerialFrameTransport::new(stream, b')', MAX_FRAME_SIZE)
            .with_read_timeout(self.timeout)
            .with_write_timeout(self.timeout);

        Ok(Box::new(transport))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn factory_clone_preserves_config() {
        let factory =
            Fp2SerialTransportFactory::new("/dev/ttyACM0", 115_200, Duration::from_secs(3));
        let cloned = factory;
        assert_eq!(cloned.port, "/dev/ttyACM0");
        assert_eq!(cloned.baud_rate, 115_200);
        assert_eq!(cloned.timeout, Duration::from_secs(3));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // tokio-serial uses syscalls Miri doesn't model
    async fn open_returns_transport_open_error_for_missing_device() {
        use std::error::Error;
        let factory = Fp2SerialTransportFactory::new(
            "/dev/nonexistent_dsd_fp2_99999",
            115_200,
            Duration::from_millis(100),
        );
        match factory.open().await {
            Err(TransportError::Open(io)) => {
                // `io::Error::other(e)` (vs `io::Error::other(e.to_string())`)
                // preserves the original `tokio_serial::Error` as the
                // io::Error's source, so log/debug output traversing
                // `Error::source()` recovers the underlying cause.
                assert!(
                    io.source().is_some() || io.get_ref().is_some(),
                    "expected the underlying tokio_serial::Error to be preserved as source"
                );
            }
            Err(other) => panic!("expected Open error, got {other:?}"),
            Ok(_) => panic!("expected open to fail for nonexistent device"),
        }
    }
}

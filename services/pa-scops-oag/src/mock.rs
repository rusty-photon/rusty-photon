//! Mock Scops OAG transport for testing without real hardware.
//!
//! Provides a [`TransportFactory`] that hands out a [`FrameTransport`] backed by
//! an in-memory state machine that mimics the Scops OAG's ASCII protocol.
//! Persists state across reconnects so tests can disconnect/reconnect and still
//! observe their prior writes (matching real hardware that keeps its position
//! when an ASCOM client cycles `Connected`).

// `#[cfg(any(feature = "mock", test))]`-gated test-helper infrastructure that
// never ships in production builds. Excluded from coverage so the workspace
// coverage number reflects only production-shipped code.
#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};
use tokio::sync::Mutex;
use tracing::debug;

/// Steps the mock advances per `A` poll while a move is in flight, and the
/// snap-to-target threshold (when the remaining distance is within this, the
/// move completes on that poll). Matches the qhy-focuser mock's model so the
/// polling-based BDD scenarios behave identically.
const STEP_PER_POLL: i64 = 1000;

/// In-memory Scops state plus a queue of frames the device "emitted".
#[derive(Debug)]
struct MockState {
    response_queue: VecDeque<Vec<u8>>,
    position: i64,
    target: Option<i64>,
    is_moving: bool,
    firmware: String,
}

impl Default for MockState {
    fn default() -> Self {
        Self {
            response_queue: VecDeque::new(),
            position: 0,
            target: None,
            is_moving: false,
            firmware: "1.2".to_string(),
        }
    }
}

impl MockState {
    fn process_command(&mut self, command_bytes: &[u8]) {
        let command = std::str::from_utf8(command_bytes)
            .unwrap_or_default()
            .trim();
        debug!(command, "mock Scops processing command");

        if command == "#" {
            self.push("OK_SCOPS");
        } else if command == "A" {
            self.advance_movement();
            let moving = i32::from(self.is_moving);
            self.push(&format!(
                "OK_SCOPS:{}:1:0:{}:{}:1:0:1:0",
                self.firmware, self.position, moving
            ));
        } else if command == "H" {
            self.is_moving = false;
            self.target = None;
            self.push("0");
        } else if let Some(rest) = command.strip_prefix("M:") {
            match parse_pos(rest) {
                Some(pos) => {
                    self.target = Some(pos);
                    self.is_moving = pos != self.position;
                    self.push(&format!("M:{pos}"));
                }
                None => self.push("ERR:"),
            }
        } else if let Some(rest) = command.strip_prefix("W:") {
            match parse_pos(rest) {
                Some(pos) => {
                    self.position = pos;
                    self.target = None;
                    self.is_moving = false;
                    self.push(&format!("W:{pos}"));
                }
                None => self.push("ERR:"),
            }
        } else {
            // Unknown command — the firmware answers `ERR:` (e.g. for the
            // unsupported `N:`/`C:` the driver never sends).
            self.push("ERR:");
        }
    }

    /// Advance the simulated move one poll's worth, snapping to target when
    /// within [`STEP_PER_POLL`]. Called on each `A` read.
    const fn advance_movement(&mut self) {
        if !self.is_moving {
            return;
        }
        if let Some(target) = self.target {
            let diff = target.saturating_sub(self.position);
            if diff.saturating_abs() <= STEP_PER_POLL {
                self.position = target;
                self.is_moving = false;
                self.target = None;
            } else if diff > 0 {
                self.position = self.position.saturating_add(STEP_PER_POLL);
            } else {
                self.position = self.position.saturating_sub(STEP_PER_POLL);
            }
        } else {
            self.is_moving = false;
        }
    }

    fn push(&mut self, response: &str) {
        debug!(response, "mock Scops queuing response");
        // Real hardware terminates with CRLF; the codec trims it.
        self.response_queue
            .push_back(format!("{response}\r\n").into_bytes());
    }
}

fn parse_pos(s: &str) -> Option<i64> {
    // Tolerate the INDI `M:<pos>d` quirk: strip at most one trailing `d` —
    // the firmware tolerates exactly that single stray byte.
    let s = s.trim();
    let s = s.strip_suffix('d').unwrap_or(s);
    s.parse().ok()
}

/// One open mock transport. Shares state with the factory so persistent device
/// settings survive a reconnect cycle.
struct MockFrameTransport {
    state: Arc<Mutex<MockState>>,
}

#[async_trait]
impl FrameTransport for MockFrameTransport {
    async fn send_frame(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        self.state.lock().await.process_command(bytes);
        Ok(())
    }

    async fn recv_frame(&mut self, buf: &mut Vec<u8>) -> Result<(), TransportError> {
        let frame = self
            .state
            .lock()
            .await
            .response_queue
            .pop_front()
            .ok_or(TransportError::Eof)?;
        buf.clear();
        buf.extend_from_slice(&frame);
        Ok(())
    }
}

/// Mock factory for the Scops OAG transport.
///
/// Maintains persistent device state across multiple open/close cycles so tests
/// can power-cycle the connection without losing the simulated device's
/// position — matching the behaviour of real hardware.
#[derive(Clone, Default)]
pub struct MockScopsTransportFactory {
    state: Arc<Mutex<MockState>>,
}

#[async_trait]
impl TransportFactory for MockScopsTransportFactory {
    async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError> {
        debug!("mock Scops transport opened");
        Ok(Box::new(MockFrameTransport {
            state: Arc::clone(&self.state),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn open(factory: &MockScopsTransportFactory) -> Box<dyn FrameTransport> {
        factory.open().await.unwrap()
    }

    async fn round_trip(t: &mut Box<dyn FrameTransport>, cmd: &[u8]) -> String {
        t.send_frame(cmd).await.unwrap();
        let mut buf = Vec::new();
        t.recv_frame(&mut buf).await.unwrap();
        String::from_utf8(buf).unwrap().trim().to_string()
    }

    #[tokio::test]
    async fn handshake_returns_ok_scops() {
        let factory = MockScopsTransportFactory::default();
        let mut t = open(&factory).await;
        assert_eq!(round_trip(&mut t, b"#\n").await, "OK_SCOPS");
    }

    #[tokio::test]
    async fn status_reports_initial_position_zero_idle() {
        let factory = MockScopsTransportFactory::default();
        let mut t = open(&factory).await;
        assert_eq!(
            round_trip(&mut t, b"A\n").await,
            "OK_SCOPS:1.2:1:0:0:0:1:0:1:0"
        );
    }

    #[tokio::test]
    async fn move_echoes_and_sets_moving() {
        let factory = MockScopsTransportFactory::default();
        let mut t = open(&factory).await;
        assert_eq!(round_trip(&mut t, b"M:20000\n").await, "M:20000");
        // First poll advances by STEP_PER_POLL and is still moving.
        assert_eq!(
            round_trip(&mut t, b"A\n").await,
            "OK_SCOPS:1.2:1:0:1000:1:1:0:1:0"
        );
    }

    #[tokio::test]
    async fn move_within_threshold_completes_on_first_poll() {
        let factory = MockScopsTransportFactory::default();
        let mut t = open(&factory).await;
        round_trip(&mut t, b"M:500\n").await;
        assert_eq!(
            round_trip(&mut t, b"A\n").await,
            "OK_SCOPS:1.2:1:0:500:0:1:0:1:0"
        );
    }

    #[tokio::test]
    async fn sync_sets_position_without_moving() {
        let factory = MockScopsTransportFactory::default();
        let mut t = open(&factory).await;
        assert_eq!(round_trip(&mut t, b"W:15000\n").await, "W:15000");
        assert_eq!(
            round_trip(&mut t, b"A\n").await,
            "OK_SCOPS:1.2:1:0:15000:0:1:0:1:0"
        );
    }

    #[tokio::test]
    async fn halt_returns_flag_and_stops() {
        let factory = MockScopsTransportFactory::default();
        let mut t = open(&factory).await;
        round_trip(&mut t, b"M:50000\n").await;
        assert_eq!(round_trip(&mut t, b"H\n").await, "0");
        // After halt the device is idle.
        assert!(round_trip(&mut t, b"A\n").await.ends_with(":0:1:0:1:0"));
    }

    #[tokio::test]
    async fn unsupported_command_returns_err() {
        let factory = MockScopsTransportFactory::default();
        let mut t = open(&factory).await;
        // `N:` (reverse) is rejected by firmware 1.2.
        assert_eq!(round_trip(&mut t, b"N:0\n").await, "ERR:");
    }

    #[tokio::test]
    async fn state_persists_across_reopens() {
        let factory = MockScopsTransportFactory::default();
        {
            let mut t = open(&factory).await;
            round_trip(&mut t, b"W:15000\n").await;
        }
        let mut t = open(&factory).await;
        assert_eq!(
            round_trip(&mut t, b"A\n").await,
            "OK_SCOPS:1.2:1:0:15000:0:1:0:1:0"
        );
    }

    #[tokio::test]
    async fn empty_queue_returns_eof() {
        let factory = MockScopsTransportFactory::default();
        let mut t = open(&factory).await;
        let mut buf = Vec::new();
        assert!(matches!(
            t.recv_frame(&mut buf).await.unwrap_err(),
            TransportError::Eof
        ));
    }

    #[tokio::test]
    async fn move_with_trailing_d_is_tolerated() {
        let factory = MockScopsTransportFactory::default();
        let mut t = open(&factory).await;
        // INDI's `snprintf("%ud")` quirk; the firmware tolerates it.
        assert_eq!(round_trip(&mut t, b"M:5000d\n").await, "M:5000");
    }

    #[tokio::test]
    async fn move_with_multiple_trailing_ds_is_rejected() {
        let factory = MockScopsTransportFactory::default();
        let mut t = open(&factory).await;
        // Only the single INDI stray byte is tolerated; anything beyond is
        // malformed and answered `ERR:` like any other unparseable position.
        assert_eq!(round_trip(&mut t, b"M:5000dd\n").await, "ERR:");
    }
}

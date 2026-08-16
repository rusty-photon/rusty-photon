//! In-process FP2 simulator used by tests and `ConformU`.
//!
//! Implements [`TransportFactory`] from
//! [`rusty_photon_shared_transport`]. Each `open()` hands out a fresh
//! [`MockFrameTransport`] sharing one [`MockState`], so multiple sessions
//! against the same factory see the same simulated device state.

// `#[cfg(feature = "mock")]`-gated test-helper infrastructure that never
// ships in production builds. Excluded from coverage so the workspace
// coverage number reflects only production-shipped code — counting these
// never-shipped mock lines would produce false coverage figures.
#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::Arc;

use async_trait::async_trait;
use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};
use tokio::sync::Mutex;

/// Shared, mutable state of the simulated FP2.
#[derive(Debug, Clone, Default)]
pub struct MockState {
    inner: Arc<Mutex<MockStateInner>>,
}

#[derive(Debug)]
struct MockStateInner {
    board: String,
    version: String,
    /// Current cover angle (0 = open, 270 = closed).
    cover_angle: u16,
    /// Pending target angle, or `None` between commands.
    target_angle: Option<u16>,
    motor_running: bool,
    light_on: bool,
    brightness: u16,
    heater_temp: f64,
    heater_mode: u8,
}

impl Default for MockStateInner {
    fn default() -> Self {
        Self {
            board: "DeepSkyDad.FP2".to_string(),
            version: "1.0.14.2".to_string(),
            // At-rest is closed (`270` angle → `[GOPS]→(0)`), matching what a
            // real FP2 reports on first connect.
            cover_angle: 270,
            target_angle: None,
            motor_running: false,
            light_on: false,
            brightness: 0,
            heater_temp: 25.0,
            heater_mode: 2,
        }
    }
}

impl MockState {
    /// Override the firmware identification returned by `[GFRM]`.
    pub async fn set_firmware(&self, board: &str, version: &str) {
        let mut inner = self.inner.lock().await;
        inner.board = board.to_string();
        inner.version = version.to_string();
    }

    /// Set the cover angle (0 = open, 270 = closed).
    pub async fn set_cover_angle(&self, angle: u16) {
        let mut inner = self.inner.lock().await;
        inner.cover_angle = angle;
        inner.motor_running = false;
    }

    /// Mark the heater as absent (firmware returns a value below -40).
    pub async fn disable_heater(&self) {
        let mut inner = self.inner.lock().await;
        inner.heater_temp = -127.0;
    }

    /// Read the current brightness as observed by the simulator.
    pub async fn brightness(&self) -> u16 {
        self.inner.lock().await.brightness
    }

    /// Read the current light state as observed by the simulator.
    pub async fn light_on(&self) -> bool {
        self.inner.lock().await.light_on
    }

    /// Read the current cover angle as observed by the simulator.
    /// 0 = open, 270 = closed, anything in between = mid-motion.
    pub async fn cover_angle(&self) -> u16 {
        self.inner.lock().await.cover_angle
    }

    /// Read the current motor-running flag as observed by the simulator.
    pub async fn motor_running(&self) -> bool {
        self.inner.lock().await.motor_running
    }
}

/// Factory producing in-process `FrameTransport`s backed by a shared [`MockState`].
#[derive(Debug, Clone, Default)]
pub struct MockTransportFactory {
    state: MockState,
}

impl MockTransportFactory {
    #[must_use]
    pub const fn with_state(state: MockState) -> Self {
        Self { state }
    }

    #[must_use]
    pub fn state(&self) -> MockState {
        self.state.clone()
    }
}

#[async_trait]
impl TransportFactory for MockTransportFactory {
    async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError> {
        Ok(Box::new(MockFrameTransport::new(self.state.clone())))
    }
}

/// `FrameTransport` that talks to a shared [`MockState`] via a queued
/// request/response loopback. The internal queue holds at most one frame
/// — `send_frame` enqueues a response and `recv_frame` consumes it.
pub struct MockFrameTransport {
    state: MockState,
    pending: Option<Vec<u8>>,
}

impl MockFrameTransport {
    const fn new(state: MockState) -> Self {
        Self {
            state,
            pending: None,
        }
    }

    async fn handle(&self, command: &str) -> String {
        let trimmed = command.trim();
        let Some(body) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
            return format!("(MOCK_BAD_FRAME:{trimmed})");
        };

        let mut inner = self.state.inner.lock().await;
        match body {
            "GFRM" => format!("(Board={}, Version={})", inner.board, inner.version),
            "GOPS" => {
                let v = if inner.motor_running {
                    255
                } else if inner.cover_angle == 0 {
                    1 // open
                } else if inner.cover_angle == 270 {
                    0 // closed
                } else {
                    255 // in-between
                };
                format!("({v})")
            }
            "GPOS" => format!("({})", inner.cover_angle),
            "GMOV" => {
                // One-shot: running for exactly the first `[GMOV]` read
                // after `[SMOV]`, then clears — mirrors pa-falcon-rotator's
                // mock (`is_moving` cleared on next `FA`). An instant
                // arrival raced the driver's optimistic `Moving` write
                // (device.rs `execute_move`) against the next poll tick;
                // see issue #423.
                let resp = format!("({})", i32::from(inner.motor_running));
                inner.motor_running = false;
                resp
            }
            "SMOV" => {
                if let Some(t) = inner.target_angle.take() {
                    inner.cover_angle = t;
                    inner.motor_running = true;
                }
                "(OK)".to_string()
            }
            "GLON" => format!("({})", i32::from(inner.light_on)),
            "GLBR" => format!("({})", inner.brightness),
            "GHTT" => format!("({:.6})", inner.heater_temp),
            "GHTM" => format!("({})", inner.heater_mode),
            other => {
                if let Some(arg) = other.strip_prefix("STRG") {
                    match arg.parse::<u16>() {
                        Ok(v) => {
                            inner.target_angle = Some(v);
                            "(OK)".to_string()
                        }
                        Err(_) => "(MOCK_BAD_ARG)".to_string(),
                    }
                } else if let Some(arg) = other.strip_prefix("SLON") {
                    match arg {
                        "0" => {
                            inner.light_on = false;
                            "(OK)".to_string()
                        }
                        "1" => {
                            inner.light_on = true;
                            "(OK)".to_string()
                        }
                        _ => "(MOCK_BAD_ARG)".to_string(),
                    }
                } else if let Some(arg) = other.strip_prefix("SLBR") {
                    match arg.parse::<u16>() {
                        Ok(v) => {
                            inner.brightness = v;
                            "(OK)".to_string()
                        }
                        Err(_) => "(MOCK_BAD_ARG)".to_string(),
                    }
                } else if let Some(arg) = other.strip_prefix("SHTM") {
                    match arg.parse::<u8>() {
                        Ok(v) => {
                            inner.heater_mode = v;
                            "(OK)".to_string()
                        }
                        Err(_) => "(MOCK_BAD_ARG)".to_string(),
                    }
                } else {
                    format!("(MOCK_UNKNOWN:{other})")
                }
            }
        }
    }
}

#[async_trait]
impl FrameTransport for MockFrameTransport {
    async fn send_frame(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        let cmd = std::str::from_utf8(bytes)
            .map_err(|e| TransportError::Framing(format!("non-UTF8 frame in mock: {e}")))?;
        let response = self.handle(cmd).await;
        self.pending = Some(response.into_bytes());
        Ok(())
    }

    async fn recv_frame(&mut self, buf: &mut Vec<u8>) -> Result<(), TransportError> {
        buf.clear();
        match self.pending.take() {
            Some(resp) => {
                buf.extend_from_slice(&resp);
                Ok(())
            }
            None => Err(TransportError::Framing(
                "mock recv without preceding send".to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn round_trip(state: &MockState, cmd: &str) -> String {
        let factory = MockTransportFactory::with_state(state.clone());
        let mut transport = factory.open().await.unwrap();
        transport.send_frame(cmd.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        transport.recv_frame(&mut buf).await.unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[tokio::test]
    async fn firmware_default_identifies_as_fp2() {
        let state = MockState::default();
        let resp = round_trip(&state, "[GFRM]").await;
        assert!(resp.starts_with("(Board=DeepSkyDad.FP2"));
    }

    #[tokio::test]
    async fn firmware_override_round_trips() {
        let state = MockState::default();
        state.set_firmware("DeepSkyDad.FP1", "1.0.0").await;
        let resp = round_trip(&state, "[GFRM]").await;
        assert!(resp.contains("DeepSkyDad.FP1"));
        assert!(resp.contains("Version=1.0.0"));
    }

    #[tokio::test]
    async fn cover_state_reflects_angle() {
        let state = MockState::default();
        state.set_cover_angle(0).await;
        assert_eq!(round_trip(&state, "[GOPS]").await, "(1)");
        state.set_cover_angle(270).await;
        assert_eq!(round_trip(&state, "[GOPS]").await, "(0)");
    }

    #[tokio::test]
    async fn move_sequence_updates_cover_angle() {
        let state = MockState::default();
        assert_eq!(round_trip(&state, "[STRG270]").await, "(OK)");
        assert_eq!(round_trip(&state, "[SMOV]").await, "(OK)");
        // Position arrives immediately...
        assert_eq!(round_trip(&state, "[GPOS]").await, "(270)");
        // ...but the motor is reported running for exactly one `[GMOV]`
        // read, so `[GOPS]` still reports "in-between" (255) until that
        // read happens.
        assert_eq!(round_trip(&state, "[GOPS]").await, "(255)");
        assert_eq!(round_trip(&state, "[GMOV]").await, "(1)");
        assert_eq!(round_trip(&state, "[GMOV]").await, "(0)");
        assert_eq!(round_trip(&state, "[GOPS]").await, "(0)");
    }

    #[tokio::test]
    async fn smov_without_a_target_does_not_start_the_motor() {
        let state = MockState::default();
        assert_eq!(round_trip(&state, "[SMOV]").await, "(OK)");
        assert_eq!(round_trip(&state, "[GMOV]").await, "(0)");
        assert_eq!(round_trip(&state, "[GOPS]").await, "(0)"); // unmoved default (closed)
    }

    #[tokio::test]
    async fn light_round_trip_updates_state() {
        let state = MockState::default();
        round_trip(&state, "[SLBR1234]").await;
        round_trip(&state, "[SLON1]").await;
        assert_eq!(round_trip(&state, "[GLBR]").await, "(1234)");
        assert_eq!(round_trip(&state, "[GLON]").await, "(1)");
        round_trip(&state, "[SLON0]").await;
        assert_eq!(round_trip(&state, "[GLON]").await, "(0)");
    }

    #[tokio::test]
    async fn disable_heater_pushes_temp_below_threshold() {
        let state = MockState::default();
        state.disable_heater().await;
        let resp = round_trip(&state, "[GHTT]").await;
        let body = resp
            .trim_start_matches('(')
            .trim_end_matches(')')
            .trim()
            .parse::<f64>()
            .unwrap();
        assert!(body < -40.0);
    }

    #[tokio::test]
    async fn bad_frame_returns_diagnostic() {
        let state = MockState::default();
        let resp = round_trip(&state, "GFRM").await;
        assert!(resp.starts_with("(MOCK_BAD_FRAME"));
    }

    #[tokio::test]
    async fn unknown_command_returns_diagnostic() {
        let state = MockState::default();
        let resp = round_trip(&state, "[NOPE]").await;
        assert!(resp.starts_with("(MOCK_UNKNOWN"));
    }

    #[tokio::test]
    async fn recv_without_send_errors() {
        let factory = MockTransportFactory::default();
        let mut transport = factory.open().await.unwrap();
        let mut buf = Vec::new();
        let err = transport.recv_frame(&mut buf).await.unwrap_err();
        assert!(matches!(err, TransportError::Framing(_)));
    }

    #[tokio::test]
    async fn state_observers_expose_simulator() {
        let state = MockState::default();
        let resp = round_trip(&state, "[SLBR0500]").await;
        assert_eq!(resp, "(OK)");
        assert_eq!(state.brightness().await, 500);
        round_trip(&state, "[SLON1]").await;
        assert!(state.light_on().await);
    }

    // ============================================================================
    // Malformed-arg defensive branches. `Command::encode()` only ever emits
    // well-formed bytes, so these arms can't fire from production today —
    // they exist as a guard against a future encoding bug. Test them
    // directly so a regression in `encode()` (or a hand-crafted bad command
    // from a test) gets a clear diagnostic instead of silently being
    // mis-interpreted by the mock.
    // ============================================================================

    #[tokio::test]
    async fn strg_with_non_numeric_arg_returns_diagnostic() {
        let state = MockState::default();
        let resp = round_trip(&state, "[STRGfoo]").await;
        assert_eq!(resp, "(MOCK_BAD_ARG)");
    }

    #[tokio::test]
    async fn slon_with_invalid_arg_returns_diagnostic() {
        let state = MockState::default();
        // Production only ever sends "0" or "1"; anything else is a bug.
        let resp = round_trip(&state, "[SLON2]").await;
        assert_eq!(resp, "(MOCK_BAD_ARG)");
    }

    #[tokio::test]
    async fn slbr_with_non_numeric_arg_returns_diagnostic() {
        let state = MockState::default();
        let resp = round_trip(&state, "[SLBRabcd]").await;
        assert_eq!(resp, "(MOCK_BAD_ARG)");
    }

    #[tokio::test]
    async fn factory_default_returns_open_transport() {
        let factory = MockTransportFactory::default();
        let mut t = factory.open().await.unwrap();
        t.send_frame(b"[GFRM]").await.unwrap();
        let mut buf = Vec::new();
        t.recv_frame(&mut buf).await.unwrap();
        assert!(!buf.is_empty());
    }
}

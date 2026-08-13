//! Mock Q-Focuser transport for testing without real hardware.
//!
//! Provides a [`TransportFactory`] that hands out a [`FrameTransport`]
//! backed by an in-memory state machine that mimics the Q-Focuser's
//! JSON protocol. Persists state across reconnects so tests can
//! disconnect/reconnect and still observe their prior writes (matches
//! the behaviour of real hardware that doesn't lose its settings when an
//! ASCOM client cycles `Connected`).

// `#[cfg(any(feature = "mock", test))]`-gated test-helper infrastructure
// that never ships in production builds. Excluded from coverage so the
// workspace coverage number reflects only production-shipped code —
// counting these never-shipped mock lines would produce false coverage
// figures.
#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};
use tokio::sync::Mutex;
use tracing::debug;

/// In-memory Q-Focuser device state plus a queue of frames the device
/// "emitted" in response to each accepted command.
#[derive(Debug, Default)]
struct MockState {
    response_queue: VecDeque<Vec<u8>>,
    device_state: MockDeviceState,
}

#[derive(Debug, Clone)]
struct MockDeviceState {
    position: i64,
    target_position: Option<i64>,
    is_moving: bool,
    speed: u8,
    reverse: bool,
    outer_temp_raw: i64,
    chip_temp_raw: i64,
    voltage_raw: i64,
    firmware_version: String,
    board_version: String,
}

impl Default for MockDeviceState {
    fn default() -> Self {
        Self {
            position: 0,
            target_position: None,
            is_moving: false,
            speed: 0,
            reverse: false,
            outer_temp_raw: 25000, // 25.0 °C
            chip_temp_raw: 30000,  // 30.0 °C
            voltage_raw: 125,      // 12.5 V
            firmware_version: "2.1.0".to_string(),
            board_version: "1.0".to_string(),
        }
    }
}

impl MockState {
    fn process_command(&mut self, command_bytes: &[u8]) {
        let command = std::str::from_utf8(command_bytes)
            .unwrap_or_default()
            .trim();
        debug!(command, "mock Q-Focuser processing command");

        let parsed: serde_json::Value = if let Ok(v) = serde_json::from_str(command) {
            v
        } else {
            debug!(command, "mock: invalid JSON command");
            self.push_frame(r#"{"error": "invalid command"}"#);
            return;
        };

        let cmd_id = parsed
            .get("cmd_id")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);

        let response = match cmd_id {
            1 => serde_json::json!({
                "idx": 1,
                "firmware_version": self.device_state.firmware_version,
                "board_version": self.device_state.board_version,
            }),
            2 => {
                // RelativeMove — mock simulates an instant move so the
                // legacy single-step BDD scenarios remain valid.
                let dir = parsed
                    .get("dir")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(1);
                let steps = parsed
                    .get("step")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
                    .cast_signed();
                let delta = if dir > 0 {
                    steps
                } else {
                    steps.saturating_neg()
                };
                self.device_state.position = self.device_state.position.saturating_add(delta);
                self.device_state.target_position = None;
                self.device_state.is_moving = false;
                serde_json::json!({"idx": 2})
            }
            3 => {
                self.device_state.is_moving = false;
                self.device_state.target_position = None;
                serde_json::json!({"idx": 3})
            }
            4 => serde_json::json!({
                "idx": 4,
                "o_t": self.device_state.outer_temp_raw,
                "c_t": self.device_state.chip_temp_raw,
                "c_r": self.device_state.voltage_raw,
            }),
            5 => {
                // GetPosition — same gradual-movement model as legacy
                // mock so polling-based completion detection still
                // works for the BDD suite.
                if self.device_state.is_moving {
                    if let Some(target) = self.device_state.target_position {
                        let diff = target.saturating_sub(self.device_state.position);
                        if diff.abs() <= 1000 {
                            self.device_state.position = target;
                            self.device_state.is_moving = false;
                            self.device_state.target_position = None;
                        } else if diff > 0 {
                            self.device_state.position =
                                self.device_state.position.saturating_add(1000);
                        } else {
                            self.device_state.position =
                                self.device_state.position.saturating_sub(1000);
                        }
                    }
                }
                serde_json::json!({"idx": 5, "pos": self.device_state.position})
            }
            6 => {
                let position = parsed
                    .get("tar")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                self.device_state.target_position = Some(position);
                self.device_state.is_moving = true;
                serde_json::json!({"idx": 6})
            }
            7 => {
                self.device_state.reverse = parsed
                    .get("rev")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
                    == 1;
                serde_json::json!({"idx": 7})
            }
            11 => {
                self.device_state.position = parsed
                    .get("init_val")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                serde_json::json!({"idx": 11})
            }
            13 => {
                // Saturate rather than truncate: a nonsense speed should
                // look like a nonsense speed, not wrap to a plausible one.
                self.device_state.speed = parsed
                    .get("speed")
                    .and_then(serde_json::Value::as_u64)
                    .map_or(0, |s| u8::try_from(s).unwrap_or(u8::MAX));
                serde_json::json!({"idx": 13})
            }
            16 => serde_json::json!({"idx": 16}),
            19 => serde_json::json!({"idx": 19}),
            other => {
                debug!(cmd_id = other, "mock: unknown cmd_id");
                serde_json::json!({"error": "unknown command"})
            }
        };

        self.push_frame(&response.to_string());
    }

    fn push_frame(&mut self, response: &str) {
        debug!(response, "mock Q-Focuser queuing response");
        self.response_queue.push_back(response.as_bytes().to_vec());
    }
}

/// One open mock transport. Shares state with the factory so persistent
/// device settings survive a reconnect cycle.
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
        let frame = self.state.lock().await.response_queue.pop_front();
        match frame {
            Some(frame) => {
                buf.clear();
                buf.extend_from_slice(&frame);
                Ok(())
            }
            None => Err(TransportError::Eof),
        }
    }
}

/// Mock factory for the Q-Focuser transport.
///
/// Maintains persistent device state across multiple open/close cycles
/// so tests can power-cycle the connection without losing the simulated
/// device's settings — matching the behaviour of real hardware.
#[derive(Clone, Default)]
pub struct MockQhyTransportFactory {
    state: Arc<Mutex<MockState>>,
}

#[async_trait]
impl TransportFactory for MockQhyTransportFactory {
    async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError> {
        debug!("mock Q-Focuser transport opened");
        Ok(Box::new(MockFrameTransport {
            state: Arc::clone(&self.state),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn open(factory: &MockQhyTransportFactory) -> Box<dyn FrameTransport> {
        factory.open().await.unwrap()
    }

    #[tokio::test]
    async fn get_version_round_trip() {
        let factory = MockQhyTransportFactory::default();
        let mut t = open(&factory).await;
        t.send_frame(br#"{"cmd_id": 1}"#).await.unwrap();
        let mut buf = Vec::new();
        t.recv_frame(&mut buf).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(value["idx"], 1);
        assert_eq!(value["firmware_version"], "2.1.0");
    }

    #[tokio::test]
    async fn get_position_returns_current_position() {
        let factory = MockQhyTransportFactory::default();
        let mut t = open(&factory).await;
        t.send_frame(br#"{"cmd_id": 5}"#).await.unwrap();
        let mut buf = Vec::new();
        t.recv_frame(&mut buf).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(value["idx"], 5);
        assert_eq!(value["pos"], 0);
    }

    #[tokio::test]
    async fn temperature_returns_raw_scaled_values() {
        let factory = MockQhyTransportFactory::default();
        let mut t = open(&factory).await;
        t.send_frame(br#"{"cmd_id": 4}"#).await.unwrap();
        let mut buf = Vec::new();
        t.recv_frame(&mut buf).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(value["o_t"], 25000);
        assert_eq!(value["c_t"], 30000);
        assert_eq!(value["c_r"], 125);
    }

    #[tokio::test]
    async fn absolute_move_sets_target_and_polling_completes() {
        let factory = MockQhyTransportFactory::default();
        let mut t = open(&factory).await;
        // Issue absolute move to 500.
        t.send_frame(br#"{"cmd_id": 6, "tar": 500}"#).await.unwrap();
        let mut buf = Vec::new();
        t.recv_frame(&mut buf).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&buf).unwrap()["idx"],
            6
        );

        // Poll position once — diff <= 1000 so the mock snaps to target.
        t.send_frame(br#"{"cmd_id": 5}"#).await.unwrap();
        t.recv_frame(&mut buf).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(value["idx"], 5);
        assert_eq!(value["pos"], 500);
    }

    #[tokio::test]
    async fn state_persists_across_reopens() {
        let factory = MockQhyTransportFactory::default();
        {
            let mut t = open(&factory).await;
            // Sync to 15000.
            t.send_frame(br#"{"cmd_id": 11, "init_val": 15000}"#)
                .await
                .unwrap();
            let mut buf = Vec::new();
            t.recv_frame(&mut buf).await.unwrap();
        }
        let mut t = open(&factory).await;
        t.send_frame(br#"{"cmd_id": 5}"#).await.unwrap();
        let mut buf = Vec::new();
        t.recv_frame(&mut buf).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(value["pos"], 15000);
    }

    #[tokio::test]
    async fn empty_queue_returns_eof() {
        let factory = MockQhyTransportFactory::default();
        let mut t = open(&factory).await;
        let mut buf = Vec::new();
        let err = t.recv_frame(&mut buf).await.unwrap_err();
        assert!(matches!(err, TransportError::Eof));
    }

    #[tokio::test]
    async fn invalid_json_queues_error_frame() {
        let factory = MockQhyTransportFactory::default();
        let mut t = open(&factory).await;
        t.send_frame(b"not json").await.unwrap();
        let mut buf = Vec::new();
        t.recv_frame(&mut buf).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(value["error"], "invalid command");
    }

    #[tokio::test]
    async fn unknown_cmd_id_queues_error_frame() {
        let factory = MockQhyTransportFactory::default();
        let mut t = open(&factory).await;
        t.send_frame(br#"{"cmd_id": 99}"#).await.unwrap();
        let mut buf = Vec::new();
        t.recv_frame(&mut buf).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(value["error"], "unknown command");
    }

    #[tokio::test]
    async fn relative_move_forward_updates_position() {
        let factory = MockQhyTransportFactory::default();
        let mut t = open(&factory).await;
        t.send_frame(br#"{"cmd_id": 2, "dir": 1, "step": 500}"#)
            .await
            .unwrap();
        let mut buf = Vec::new();
        t.recv_frame(&mut buf).await.unwrap();
        t.send_frame(br#"{"cmd_id": 5}"#).await.unwrap();
        t.recv_frame(&mut buf).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(value["pos"], 500);
    }

    #[tokio::test]
    async fn relative_move_backward_updates_position() {
        let factory = MockQhyTransportFactory::default();
        let mut t = open(&factory).await;
        // Move forward 1000, then back 300.
        t.send_frame(br#"{"cmd_id": 2, "dir": 1, "step": 1000}"#)
            .await
            .unwrap();
        let mut buf = Vec::new();
        t.recv_frame(&mut buf).await.unwrap();
        t.send_frame(br#"{"cmd_id": 2, "dir": -1, "step": 300}"#)
            .await
            .unwrap();
        t.recv_frame(&mut buf).await.unwrap();
        t.send_frame(br#"{"cmd_id": 5}"#).await.unwrap();
        t.recv_frame(&mut buf).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(value["pos"], 700);
    }

    #[tokio::test]
    async fn set_speed_set_reverse_set_pdn_set_hold_all_ack() {
        let factory = MockQhyTransportFactory::default();
        let mut t = open(&factory).await;
        let mut buf = Vec::new();
        // Typed on the binding: the rows are byte-string literals of
        // differing length, so the element type has to be named
        // somewhere for them to unsize to a common `&[u8]`.
        let cases: &[(&[u8], u64)] = &[
            (br#"{"cmd_id": 13, "speed": 5}"#, 13),
            (br#"{"cmd_id": 7, "rev": 1}"#, 7),
            (br#"{"cmd_id": 16, "ihold": 10, "irun": 20}"#, 16),
            (br#"{"cmd_id": 19, "pdn_d": 1}"#, 19),
            (br#"{"cmd_id": 3}"#, 3),
        ];
        for &(cmd, idx) in cases {
            t.send_frame(cmd).await.unwrap();
            t.recv_frame(&mut buf).await.unwrap();
            let value: serde_json::Value = serde_json::from_slice(&buf).unwrap();
            assert_eq!(value["idx"], idx, "cmd {cmd:?} should ack with idx {idx}");
        }
    }
}

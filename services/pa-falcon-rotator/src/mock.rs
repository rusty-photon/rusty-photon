//! Mock Falcon transport for testing without real hardware.
//!
//! Feature-gated under `mock`. Provides a
//! [`rusty_photon_shared_transport::TransportFactory`] that hands out an
//! in-memory [`rusty_photon_shared_transport::FrameTransport`] backed by a
//! deterministic state machine. The state machine responds to every
//! Falcon command from the design-doc
//! [Command Table](../../../docs/services/falcon-rotator.md#command-table)
//! and tracks `is_moving` / position / reverse / derotation across
//! commands; tests inspect `command_log` to assert wire traffic.
//!
//! `FF` (firmware reload) is deliberately rejected with an error sentinel
//! — the design doc forbids the driver from ever issuing it, and the mock
//! refuses to silently accept it so a routing regression fails loudly.
//!
//! State persists across `open()` cycles to mirror real hardware: the
//! Falcon's EEPROM keeps its `motor_reverse` setting and the mechanical
//! position survives a power-cycle.

// `#[cfg(feature = "mock")]`-gated test-helper infrastructure that never
// ships in production builds. Excluded from coverage so the workspace
// coverage number reflects only production-shipped code — counting these
// never-shipped mock lines would produce false coverage figures.
#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};
use tokio::sync::Mutex;
use tracing::debug;

use crate::protocol::{FalconMotion, FalconSettings};
use crate::units::{MechanicalDegrees, Steps, STEPS_PER_DEGREE};

/// Firmware-enforced CW soft limit, in degrees. A target beyond this is only
/// reachable the long way round (CCW past the 0° home), which the firmware
/// reports as a *negative* signed step count. Modelled here so the mock
/// reproduces the real-hardware behaviour `ConformU` exercises (firmware 1.5).
const FALCON_CW_LIMIT_DEG: f64 = 220.0;

/// Default raw ADC voltage reading returned by `VS`.
const DEFAULT_VOLTAGE_RAW: u32 = 800;

/// Default firmware version reported by `FV`.
const DEFAULT_FIRMWARE_VERSION: &str = "1.3";

/// Sentinel returned for commands the driver should never issue (e.g.
/// `FF`) or that the mock does not understand. Surfaces as
/// `InvalidResponse` in the driver layer and trips a failing test.
const UNKNOWN_COMMAND_RESPONSE: &str = "ERR:UNKNOWN";

/// Simulated Falcon Rotator device state.
#[derive(Debug, Clone)]
struct MockDeviceState {
    /// Mechanical position the device currently holds, in its own frame.
    /// The signed `FA`/`FP` step counter is *derived* from this via
    /// [`signed_target_steps`] (negative CCW of the 0° home, for targets past
    /// the 220° CW limit), mirroring how the real Falcon reports a step count
    /// alongside the authoritative degree field.
    mech: MechanicalDegrees,
    /// Live `FA` state. `is_moving` stays `true` until the next `FA`
    /// clears it (best-effort BDD model); `limit_detect` is set
    /// directly by test hooks.
    motion: FalconMotion,
    /// Persisted settings. `motor_reverse` mirrors `FN:b`;
    /// `do_derotation` is `true` after `DR:<ms>` where `ms > 0` and
    /// cleared by `DR:0`.
    settings: FalconSettings,
    /// Raw ADC count returned by `VS`.
    voltage_raw: u32,
    /// String returned by `FV`.
    firmware_version: String,
}

impl Default for MockDeviceState {
    fn default() -> Self {
        Self {
            mech: MechanicalDegrees::new(0.0),
            motion: FalconMotion {
                is_moving: false,
                limit_detect: false,
            },
            settings: FalconSettings {
                do_derotation: false,
                motor_reverse: false,
            },
            voltage_raw: DEFAULT_VOLTAGE_RAW,
            firmware_version: DEFAULT_FIRMWARE_VERSION.to_string(),
        }
    }
}

impl MockDeviceState {
    /// The signed step counter the device reports for `FP` and the `FA` step
    /// field, derived from the mechanical angle (negative CCW of the 0° home;
    /// see [`signed_target_steps`]).
    fn position_steps(&self) -> Steps {
        signed_target_steps(self.mech)
    }

    /// The `[0, 360)` mechanical angle the firmware reports for `FD` and the
    /// `FA` degree field.
    const fn mech_position_deg(&self) -> f64 {
        self.mech.value()
    }

    fn full_status_response(&self) -> String {
        format!(
            "FR_OK:{}:{:.2}:{}:{}:{}:{}",
            self.position_steps().value(),
            self.mech_position_deg(),
            bit(self.motion.is_moving),
            bit(self.motion.limit_detect),
            bit(self.settings.do_derotation),
            bit(self.settings.motor_reverse),
        )
    }
}

fn bit(b: bool) -> u8 {
    u8::from(b)
}

/// Map a mechanical angle to the Falcon's *signed* step counter, modelling the
/// 220° CW soft limit: a target beyond 220° is only reachable the long way
/// round (CCW past the 0° home), which the firmware represents as a negative
/// step count (`deg - 360`). Mirrors real-hardware capture (firmware 1.5).
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "`MechanicalDegrees` is normalized to [0, 360), so `signed` lies in (-140, 220] and the product in (-12_124, 19_052] — five decimal orders inside `i32`. `f64` to `i32` has no total spelling, and a fallible one would add a dead arm whose fallback misplaces the rotator"
)]
fn signed_target_steps(mech: MechanicalDegrees) -> Steps {
    let d = mech.value();
    let signed = if d > FALCON_CW_LIMIT_DEG {
        d - 360.0
    } else {
        d
    };
    Steps((signed * STEPS_PER_DEGREE).round() as i32)
}

/// In-memory Falcon state plus a queue of pending response frames.
#[derive(Debug, Default)]
struct MockState {
    response_queue: VecDeque<Vec<u8>>,
    device_state: MockDeviceState,
    command_log: Vec<String>,
}

impl MockState {
    fn process_command(&mut self, command_bytes: &[u8]) {
        let raw = std::str::from_utf8(command_bytes).unwrap_or_default();
        let command = raw.trim_end_matches(['\r', '\n']).trim();
        if command.is_empty() {
            return;
        }
        debug!(command, "mock processing command");
        self.command_log.push(command.to_string());

        let response = match command {
            "F#" => "FR_OK".to_string(),
            "FA" => {
                let resp = self.device_state.full_status_response();
                // Plan §3b: is_moving "best-effort — flipped by MD:, cleared
                // on next FA after each move". Clearing here makes the
                // sequence MD → FA(moving=1) → FA(moving=0) the BDD path.
                self.device_state.motion.is_moving = false;
                resp
            }
            "FV" => format!("FV:{}", self.device_state.firmware_version),
            "FD" => format!("FD:{:.2}", self.device_state.mech_position_deg()),
            "FP" => format!("FP:{}", self.device_state.position_steps().value()),
            "VS" => format!("VS:{}", self.device_state.voltage_raw),
            "FH" => {
                self.device_state.motion.is_moving = false;
                "FH:1".to_string()
            }
            "FR" => format!("FR:{}", bit(self.device_state.motion.is_moving)),
            "FF" => UNKNOWN_COMMAND_RESPONSE.to_string(),
            other if other.starts_with("DR:") => self.process_derotation(other),
            other if other.starts_with("MD:") => self.process_move_deg(other),
            other if other.starts_with("MS:") => self.process_move_steps(other),
            other if other.starts_with("FN:") => self.process_reverse(other),
            _ => UNKNOWN_COMMAND_RESPONSE.to_string(),
        };

        debug!(response, "mock queuing response");
        let mut frame = response.into_bytes();
        frame.push(b'\n');
        self.response_queue.push_back(frame);
    }

    fn process_derotation(&mut self, command: &str) -> String {
        let value = command.strip_prefix("DR:").unwrap_or("");
        match value.parse::<u32>() {
            Ok(0) => {
                self.device_state.settings.do_derotation = false;
                "DR:0".to_string()
            }
            Ok(ms) => {
                self.device_state.settings.do_derotation = true;
                format!("DR:{ms}")
            }
            Err(_) => UNKNOWN_COMMAND_RESPONSE.to_string(),
        }
    }

    fn process_move_deg(&mut self, command: &str) -> String {
        let value = command.strip_prefix("MD:").unwrap_or("");
        match value.parse::<f64>() {
            Ok(deg) if deg.is_finite() => {
                self.device_state.mech = MechanicalDegrees::new(deg);
                self.device_state.motion.is_moving = true;
                // Echo the command literally so the driver's echo
                // validation sees what it sent regardless of mock precision.
                command.to_string()
            }
            _ => UNKNOWN_COMMAND_RESPONSE.to_string(),
        }
    }

    fn process_move_steps(&mut self, command: &str) -> String {
        let value = command.strip_prefix("MS:").unwrap_or("");
        // The encoder is signed relative to the 0° home, so parse the target
        // as i32 (no lossy u32→i32 cast) and store the equivalent mechanical
        // angle; the FA/FP step field is re-derived from it.
        match value.parse::<i32>() {
            Ok(steps) => {
                self.device_state.mech = MechanicalDegrees::from(Steps(steps));
                self.device_state.motion.is_moving = true;
                format!("MS:{steps}")
            }
            Err(_) => UNKNOWN_COMMAND_RESPONSE.to_string(),
        }
    }

    fn process_reverse(&mut self, command: &str) -> String {
        let value = command.strip_prefix("FN:").unwrap_or("");
        match value {
            "0" => {
                self.device_state.settings.motor_reverse = false;
                "FN:0".to_string()
            }
            "1" => {
                self.device_state.settings.motor_reverse = true;
                "FN:1".to_string()
            }
            _ => UNKNOWN_COMMAND_RESPONSE.to_string(),
        }
    }
}

/// One open mock transport. Shares state with the factory so persistent
/// device settings (mechanical position, `motor_reverse`, …) survive across
/// reconnect cycles.
struct MockFalconFrameTransport {
    state: Arc<Mutex<MockState>>,
}

#[async_trait]
impl FrameTransport for MockFalconFrameTransport {
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

/// Mock factory for the Falcon serial transport.
///
/// Maintains persistent device state across multiple open/close cycles so
/// reconnect scenarios start from where the previous session left off,
/// matching real hardware where the Falcon's EEPROM keeps its
/// `motor_reverse` setting and the mechanical position survives a power
/// cycle.
#[derive(Clone, Debug, Default)]
pub struct MockFalconTransportFactory {
    state: Arc<Mutex<MockState>>,
}

#[async_trait]
impl TransportFactory for MockFalconTransportFactory {
    async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError> {
        debug!("mock Falcon transport opened");
        Ok(Box::new(MockFalconFrameTransport {
            state: Arc::clone(&self.state),
        }))
    }
}

impl MockFalconTransportFactory {
    /// Seed the mock's mechanical position before opening a connection.
    pub async fn set_mech_position_deg(&self, value: f64) {
        self.state.lock().await.device_state.mech = MechanicalDegrees::new(value);
    }

    /// Read the mock's current mechanical position. Used by tests that
    /// verify a code path *did not* mutate the device counter (e.g. ASCOM
    /// Sync, which must leave `MechanicalPosition` untouched).
    pub async fn mech_position_deg(&self) -> f64 {
        self.state.lock().await.device_state.mech_position_deg()
    }

    /// Seed the mock's `motor_reverse` flag (mirrors EEPROM persistence).
    pub async fn set_motor_reverse(&self, value: bool) {
        self.state.lock().await.device_state.settings.motor_reverse = value;
    }

    /// Seed the mock's `limit_detect` flag visible to the next `FA`.
    pub async fn set_limit_detect(&self, value: bool) {
        self.state.lock().await.device_state.motion.limit_detect = value;
    }

    /// Seed the raw ADC count returned by `VS`.
    pub async fn set_voltage_raw(&self, value: u32) {
        self.state.lock().await.device_state.voltage_raw = value;
    }

    /// Snapshot every command the writer has seen, in order received.
    pub async fn command_log(&self) -> Vec<String> {
        self.state.lock().await.command_log.clone()
    }

    /// Clear the recorded command log. Useful between handshake and the
    /// command-under-test in unit tests so assertions only see the latter.
    pub async fn clear_command_log(&self) {
        self.state.lock().await.command_log.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn open(factory: &MockFalconTransportFactory) -> Box<dyn FrameTransport> {
        factory.open().await.unwrap()
    }

    async fn round_trip(transport: &mut Box<dyn FrameTransport>, command: &[u8]) -> Vec<u8> {
        transport.send_frame(command).await.unwrap();
        let mut buf = Vec::new();
        transport.recv_frame(&mut buf).await.unwrap();
        buf
    }

    // ---- Helpers ---------------------------------------------------------

    #[test]
    fn bit_maps_true_and_false() {
        assert_eq!(bit(true), 1);
        assert_eq!(bit(false), 0);
    }

    // ---- Per-command tests ----------------------------------------------

    #[tokio::test]
    async fn ping_returns_fr_ok() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        let resp = round_trip(&mut t, b"F#\n").await;
        assert_eq!(&resp, b"FR_OK\n");
    }

    #[tokio::test]
    async fn firmware_version_returns_default() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        let resp = round_trip(&mut t, b"FV\n").await;
        assert_eq!(&resp, b"FV:1.3\n");
    }

    #[tokio::test]
    async fn full_status_default_shape() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        let resp = round_trip(&mut t, b"FA\n").await;
        assert_eq!(&resp, b"FR_OK:0:0.00:0:0:0:0\n");
    }

    #[tokio::test]
    async fn move_deg_updates_position_and_flags_is_moving() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        let echo = round_trip(&mut t, b"MD:50.00\n").await;
        assert_eq!(&echo, b"MD:50.00\n");

        // First FA after the move sees is_moving=1
        let first = round_trip(&mut t, b"FA\n").await;
        let first_text = std::str::from_utf8(&first).unwrap();
        assert!(
            first_text.contains(":1:0:0:0"),
            "expected moving flag set: {first_text}"
        );
        assert!(
            first_text.contains(":50.00:"),
            "expected position 50.00: {first_text}"
        );

        // Second FA returns is_moving=0
        let second = round_trip(&mut t, b"FA\n").await;
        let second_text = std::str::from_utf8(&second).unwrap();
        assert!(
            second_text.contains(":0:0:0:0"),
            "expected moving flag cleared on second poll: {second_text}"
        );
    }

    #[tokio::test]
    async fn move_steps_converts_via_steps_per_degree() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        let echo = round_trip(&mut t, b"MS:8660\n").await;
        assert_eq!(&echo, b"MS:8660\n");

        // Drain the is_moving=1 read first.
        let _ = round_trip(&mut t, b"FA\n").await;

        let pos = round_trip(&mut t, b"FD\n").await;
        assert_eq!(&pos, b"FD:100.00\n");
    }

    #[tokio::test]
    async fn halt_clears_is_moving_and_echoes_fh_one() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        // Kick a move so is_moving is set.
        let _ = round_trip(&mut t, b"MD:90.00\n").await;

        let echo = round_trip(&mut t, b"FH\n").await;
        assert_eq!(&echo, b"FH:1\n");

        let status = round_trip(&mut t, b"FA\n").await;
        let status_text = std::str::from_utf8(&status).unwrap();
        assert!(
            status_text.contains(":0:0:0:0"),
            "expected is_moving cleared after halt: {status_text}"
        );
    }

    #[tokio::test]
    async fn is_running_reports_state() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        assert_eq!(&round_trip(&mut t, b"FR\n").await, b"FR:0\n");

        let _ = round_trip(&mut t, b"MD:10.00\n").await;
        assert_eq!(&round_trip(&mut t, b"FR\n").await, b"FR:1\n");
    }

    #[tokio::test]
    async fn set_reverse_persists_state() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        assert_eq!(&round_trip(&mut t, b"FN:1\n").await, b"FN:1\n");

        let status = round_trip(&mut t, b"FA\n").await;
        let text = std::str::from_utf8(&status).unwrap();
        assert!(
            text.trim().ends_with(":1"),
            "expected motor_reverse=1: {text}"
        );

        assert_eq!(&round_trip(&mut t, b"FN:0\n").await, b"FN:0\n");
        let status = round_trip(&mut t, b"FA\n").await;
        let text = std::str::from_utf8(&status).unwrap();
        assert!(
            text.trim().ends_with(":0"),
            "expected motor_reverse=0: {text}"
        );
    }

    #[tokio::test]
    async fn derotation_off_then_on_toggles_flag() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        assert_eq!(&round_trip(&mut t, b"DR:0\n").await, b"DR:0\n");
        let status = round_trip(&mut t, b"FA\n").await;
        let text = std::str::from_utf8(&status).unwrap();
        assert!(
            text.contains(":0:0:0"),
            "expected derotation cleared: {text}"
        );

        assert_eq!(&round_trip(&mut t, b"DR:25\n").await, b"DR:25\n");
        let status = round_trip(&mut t, b"FA\n").await;
        let text = std::str::from_utf8(&status).unwrap();
        // Field order: is_moving:limit:derot:reverse → the derot bit is third.
        assert!(
            text.trim().ends_with(":0:0:1:0"),
            "expected derotation set: {text}"
        );
    }

    #[tokio::test]
    async fn voltage_returns_default_raw() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        assert_eq!(&round_trip(&mut t, b"VS\n").await, b"VS:800\n");
    }

    #[tokio::test]
    async fn position_deg_and_steps_track_each_other() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        let _ = round_trip(&mut t, b"MD:50.00\n").await;
        // Drain is_moving=1.
        let _ = round_trip(&mut t, b"FA\n").await;

        let deg = round_trip(&mut t, b"FD\n").await;
        let steps = round_trip(&mut t, b"FP\n").await;
        assert_eq!(&deg, b"FD:50.00\n");
        // 50 * 86.6 = 4330
        assert_eq!(&steps, b"FP:4330\n");
    }

    #[tokio::test]
    async fn move_past_cw_limit_reports_negative_steps() {
        // Real hardware (firmware 1.5): a target beyond the 220° CW soft limit
        // is reached the long way round (CCW past the 0° home), so the signed
        // step counter goes negative even though position_deg wraps into
        // [0, 360). This is the case the u32→i32 parse fix must survive — and
        // exactly what the real-hardware ConformU run tripped on.
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        let _ = round_trip(&mut t, b"MD:300.00\n").await;

        // 300° > 220° CW limit → signed -60° → -60 * 86.6 = -5196 steps.
        let fp = round_trip(&mut t, b"FP\n").await;
        assert_eq!(std::str::from_utf8(&fp).unwrap().trim(), "FP:-5196");

        let fa = round_trip(&mut t, b"FA\n").await;
        let fa_text = std::str::from_utf8(&fa).unwrap();
        assert!(
            fa_text.starts_with("FR_OK:-5196:300.00:"),
            "expected negative steps with wrapped degrees: {fa_text}"
        );
    }

    #[tokio::test]
    async fn limit_detect_visible_when_set() {
        let factory = MockFalconTransportFactory::default();
        factory.set_limit_detect(true).await;
        let mut t = open(&factory).await;

        let status = round_trip(&mut t, b"FA\n").await;
        let text = std::str::from_utf8(&status).unwrap();
        // Field order after FR_OK: steps:deg:moving:limit:derot:reverse
        assert!(text.contains(":0:1:0:0"), "expected limit bit set: {text}");
    }

    // ---- Defensive paths -------------------------------------------------

    #[tokio::test]
    async fn ff_returns_unknown_sentinel() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        // The design doc bans the driver from ever issuing FF. The mock
        // refuses to silently accept it so a routing regression fails loudly.
        let resp = round_trip(&mut t, b"FF\n").await;
        let text = std::str::from_utf8(&resp).unwrap().trim();
        assert_eq!(text, UNKNOWN_COMMAND_RESPONSE);
    }

    #[tokio::test]
    async fn unknown_command_returns_sentinel() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        let resp = round_trip(&mut t, b"XX\n").await;
        let text = std::str::from_utf8(&resp).unwrap().trim();
        assert_eq!(text, UNKNOWN_COMMAND_RESPONSE);
    }

    #[tokio::test]
    async fn malformed_move_deg_returns_sentinel() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        let resp = round_trip(&mut t, b"MD:not_a_float\n").await;
        let text = std::str::from_utf8(&resp).unwrap().trim();
        assert_eq!(text, UNKNOWN_COMMAND_RESPONSE);
    }

    #[tokio::test]
    async fn malformed_derotation_rate_returns_sentinel() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        let resp = round_trip(&mut t, b"DR:not_a_number\n").await;
        let text = std::str::from_utf8(&resp).unwrap().trim();
        assert_eq!(text, UNKNOWN_COMMAND_RESPONSE);
    }

    #[tokio::test]
    async fn malformed_move_steps_returns_sentinel() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        let resp = round_trip(&mut t, b"MS:not_a_number\n").await;
        let text = std::str::from_utf8(&resp).unwrap().trim();
        assert_eq!(text, UNKNOWN_COMMAND_RESPONSE);
    }

    #[tokio::test]
    async fn malformed_reverse_value_returns_sentinel() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        let resp = round_trip(&mut t, b"FN:maybe\n").await;
        let text = std::str::from_utf8(&resp).unwrap().trim();
        assert_eq!(text, UNKNOWN_COMMAND_RESPONSE);
    }

    #[tokio::test]
    async fn writer_trims_trailing_newline_from_caller() {
        // `MockFalconFrameTransport::send_frame` writes verbatim, but
        // `process_command` trims trailing CR/LF, so the mock should
        // recognise the command regardless of trailing LF/CR/CRLF.
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        t.send_frame(b"F#\r\n").await.unwrap();
        let mut buf = Vec::new();
        t.recv_frame(&mut buf).await.unwrap();
        assert_eq!(&buf, b"FR_OK\n");
    }

    #[tokio::test]
    async fn command_log_records_every_command_in_order() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;

        let _ = round_trip(&mut t, b"F#\n").await;
        let _ = round_trip(&mut t, b"FA\n").await;
        let _ = round_trip(&mut t, b"MD:45.00\n").await;

        let log = factory.command_log().await;
        assert_eq!(
            log,
            vec!["F#".to_string(), "FA".to_string(), "MD:45.00".to_string()]
        );
    }

    #[tokio::test]
    async fn factory_state_persists_across_reopens() {
        let factory = MockFalconTransportFactory::default();

        {
            let mut t = open(&factory).await;
            let _ = round_trip(&mut t, b"FN:1\n").await;
        }

        let mut t = open(&factory).await;
        let resp = round_trip(&mut t, b"FA\n").await;
        let text = std::str::from_utf8(&resp).unwrap();
        assert!(
            text.trim().ends_with(":1"),
            "expected motor_reverse to persist across reopen: {text}"
        );
    }

    #[tokio::test]
    async fn empty_queue_returns_eof() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        let mut buf = Vec::new();
        let err = t.recv_frame(&mut buf).await.unwrap_err();
        assert!(matches!(err, TransportError::Eof));
    }

    #[tokio::test]
    async fn empty_command_is_ignored() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        t.send_frame(b"\n").await.unwrap();
        // No response queued for an empty command — recv yields EOF.
        let mut buf = Vec::new();
        let err = t.recv_frame(&mut buf).await.unwrap_err();
        assert!(matches!(err, TransportError::Eof));
    }

    #[tokio::test]
    async fn clear_command_log_resets_to_empty() {
        let factory = MockFalconTransportFactory::default();
        let mut t = open(&factory).await;
        let _ = round_trip(&mut t, b"F#\n").await;
        assert_eq!(factory.command_log().await.len(), 1);
        factory.clear_command_log().await;
        assert_eq!(factory.command_log().await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn set_voltage_raw_round_trip_through_vs() {
        let factory = MockFalconTransportFactory::default();
        factory.set_voltage_raw(812).await;
        let mut t = open(&factory).await;
        let resp = round_trip(&mut t, b"VS\n").await;
        assert_eq!(&resp, b"VS:812\n");
    }

    #[tokio::test]
    async fn set_mech_position_deg_seeds_full_status() {
        let factory = MockFalconTransportFactory::default();
        factory.set_mech_position_deg(123.45).await;
        let mut t = open(&factory).await;
        let resp = round_trip(&mut t, b"FA\n").await;
        let text = std::str::from_utf8(&resp).unwrap();
        assert!(text.contains(":123.45:"), "got: {text}");
    }
}

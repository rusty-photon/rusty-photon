//! Frame codec for the Falcon Rotator serial protocol.
//!
//! Plugs into [`rusty_photon_shared_transport::SharedTransport`] as the
//! per-service [`Codec`]: encodes typed commands into LF-terminated ASCII
//! frames and decodes incoming frames into typed responses. Falcon has
//! exactly one response per request and no unsolicited frames, so
//! [`Codec::matches`] only enforces variant-shape pairing and
//! [`Codec::max_skip`] keeps its default of 0.
//!
//! Echo content validation (e.g. checking that `MD:180.00` came back with
//! `MD:180.00`, not `MD:181.00`) is the manager's job — the codec hands
//! back the [`FalconResponse::Echo`] variant verbatim and
//! [`crate::manager::FalconManager`] cross-checks the string via
//! [`crate::protocol::validate_echo`].

use rusty_photon_shared_transport::{Codec, SessionError, TransportError};
use std::str::Utf8Error;
use thiserror::Error;

use crate::error::FalconRotatorError;
use crate::protocol::{
    parse_firmware_version, parse_is_running, parse_position_deg, parse_position_steps,
    parse_voltage_raw, Command, FalconStatus,
};
use crate::units::{MechanicalDegrees, Steps};

/// Decoded response frame from the device.
///
/// One variant per documented response shape. `Echo` covers all the
/// echo-bearing wire replies (`MD:`, `MS:`, `DR:`, `FH:1`, `FN:`); the
/// manager validates the echo's content against the issued command.
#[derive(Debug, Clone, PartialEq)]
pub enum FalconResponse {
    /// `FR_OK` — the ping ack.
    Ack,
    /// `FR_OK:steps:deg:moving:limit:derot:reverse` — the `FA` full-status reply.
    Status(FalconStatus),
    /// `FV:n.n` — the `FV` firmware-version reply.
    FirmwareVersion(String),
    /// `FD:nn.nn` — the `FD` position-in-degrees reply.
    PositionDeg(MechanicalDegrees),
    /// `FP:n..` — the `FP` signed position-in-steps reply (negative CCW of
    /// the 0° home; see [`crate::protocol::FalconStatus::position_steps`]).
    PositionSteps(Steps),
    /// `VS:n..` — the `VS` raw input-voltage reply.
    Voltage(u32),
    /// `FR:0` / `FR:1` — the `FR` is-running reply.
    IsRunning(bool),
    /// `MD:nn.nn` / `MS:n` / `DR:n` / `FH:1` / `FN:b` — echo-bearing replies.
    /// The manager checks the echo's content against the issued command.
    Echo(String),
}

/// Codec-side error type.
///
/// Carries enough variants to flatten a full
/// [`SessionError<FalconCodecError>`] in handshake / poll-style contexts
/// without losing the structural information the device-layer mapping
/// (`From<SessionError<…>> for FalconRotatorError`) then re-expands into
/// the right `FalconRotatorError` variant.
///
/// `Transport` carries the underlying [`TransportError`] structurally
/// rather than as a string so a transport-level failure surfaced
/// *through* the handshake hook (which returns
/// `Result<_, FalconCodecError>`) can still be classified as `Open` /
/// `Io` / `Timeout` / `Eof` / `Framing` by the device layer instead of
/// collapsing to a generic `Communication` error at connect time.
#[derive(Debug, Error)]
pub enum FalconCodecError {
    #[error("invalid UTF-8 in response: {0}")]
    Utf8(#[from] Utf8Error),
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error(transparent)]
    Transport(TransportError),
    #[error("device returned non-matching response ({0} frame(s) read)")]
    SkipExhausted(usize),
}

impl FalconCodecError {
    /// Tighten a protocol-layer [`FalconRotatorError`] into the
    /// codec-side error type. Used inside [`Codec::decode`] so any
    /// parser-error variant the protocol module returns gets reshaped
    /// into the codec's narrow enum.
    fn from_protocol(err: FalconRotatorError) -> Self {
        match err {
            FalconRotatorError::InvalidResponse(s) => Self::InvalidResponse(s),
            FalconRotatorError::ParseError(s) => Self::Parse(s),
            other => Self::InvalidResponse(other.to_string()),
        }
    }
}

impl From<SessionError<Self>> for FalconCodecError {
    fn from(err: SessionError<Self>) -> Self {
        match err {
            SessionError::Transport(t) => Self::Transport(t),
            SessionError::Codec(c) => c,
            SessionError::SkipExhausted(n) => Self::SkipExhausted(n),
        }
    }
}

/// Zero-sized codec for the Falcon Rotator wire protocol.
#[derive(Debug, Clone, Copy, Default)]
pub struct FalconCodec;

impl Codec for FalconCodec {
    type Command = Command;
    type Response = FalconResponse;
    type Error = FalconCodecError;

    fn encode(&self, cmd: &Self::Command) -> Vec<u8> {
        let mut bytes = cmd.to_command_string().into_bytes();
        bytes.push(b'\n');
        bytes
    }

    fn decode(&self, bytes: &[u8]) -> Result<Self::Response, Self::Error> {
        let text = std::str::from_utf8(bytes)?.trim();

        // Order matters: the exact-match `FR_OK` ping ack must take precedence
        // over the `FR_OK:` full-status prefix, and `FR:` must not be confused
        // with `FR_OK` (the `_OK` suffix discriminates them).
        if text == "FR_OK" {
            return Ok(FalconResponse::Ack);
        }
        if text.starts_with("FR_OK:") {
            return text
                .parse::<FalconStatus>()
                .map(FalconResponse::Status)
                .map_err(FalconCodecError::from_protocol);
        }
        if text.starts_with("FV:") {
            return parse_firmware_version(text)
                .map(FalconResponse::FirmwareVersion)
                .map_err(FalconCodecError::from_protocol);
        }
        if text.starts_with("FD:") {
            return parse_position_deg(text)
                .map(FalconResponse::PositionDeg)
                .map_err(FalconCodecError::from_protocol);
        }
        if text.starts_with("FP:") {
            return parse_position_steps(text)
                .map(FalconResponse::PositionSteps)
                .map_err(FalconCodecError::from_protocol);
        }
        if text.starts_with("VS:") {
            return parse_voltage_raw(text)
                .map(FalconResponse::Voltage)
                .map_err(FalconCodecError::from_protocol);
        }
        if text.starts_with("FR:") {
            return parse_is_running(text)
                .map(FalconResponse::IsRunning)
                .map_err(FalconCodecError::from_protocol);
        }
        // Echo-bearing wire replies (MD:, MS:, DR:, FH:1, FN:). The manager
        // validates the echo's content against the issued command — the
        // codec just hands it back verbatim.
        Ok(FalconResponse::Echo(text.to_string()))
    }

    fn matches(&self, cmd: &Self::Command, resp: &Self::Response) -> bool {
        matches!(
            (cmd, resp),
            (Command::Ping, FalconResponse::Ack)
                | (Command::FullStatus, FalconResponse::Status(_))
                | (Command::FirmwareVersion, FalconResponse::FirmwareVersion(_))
                | (Command::PositionDeg, FalconResponse::PositionDeg(_))
                | (Command::PositionSteps, FalconResponse::PositionSteps(_))
                | (Command::Voltage, FalconResponse::Voltage(_))
                | (Command::IsRunning, FalconResponse::IsRunning(_))
                | (
                    Command::DerotationOff
                        | Command::DerotationRate(_)
                        | Command::MoveDeg(_)
                        | Command::MoveSteps(_)
                        | Command::Halt
                        | Command::SetReverse(_),
                    FalconResponse::Echo(_),
                )
        )
    }
}

/// Bridge [`SessionError<FalconCodecError>`] into
/// [`FalconRotatorError`]. The mapping pins which ASCOM code each failure
/// surfaces as via [`FalconRotatorError::to_ascom_error`].
impl From<SessionError<FalconCodecError>> for FalconRotatorError {
    fn from(err: SessionError<FalconCodecError>) -> Self {
        match err {
            // Both alternatives route through `From<TransportError> for
            // FalconRotatorError` in error.rs so a timeout that surfaces
            // *through* the handshake hook (codec alternative) gets the
            // same classification as one that surfaces on a steady-state
            // request (transport alternative).
            SessionError::Transport(t) | SessionError::Codec(FalconCodecError::Transport(t)) => {
                t.into()
            }
            SessionError::Codec(FalconCodecError::InvalidResponse(s)) => Self::InvalidResponse(s),
            SessionError::Codec(FalconCodecError::Parse(s)) => Self::ParseError(s),
            SessionError::Codec(c @ FalconCodecError::Utf8(_)) => {
                Self::InvalidResponse(c.to_string())
            }
            SessionError::Codec(FalconCodecError::SkipExhausted(n))
            | SessionError::SkipExhausted(n) => Self::Communication(format!(
                "device returned non-matching response ({n} frame(s) read)"
            )),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::protocol::{FalconMotion, FalconSettings};

    // ---- encode -----------------------------------------------------------

    #[test]
    fn encode_appends_newline_terminator() {
        let bytes = FalconCodec.encode(&Command::Ping);
        assert_eq!(&bytes, b"F#\n");
    }

    #[test]
    fn encode_move_deg_uses_two_decimal_places() {
        let bytes = FalconCodec.encode(&Command::MoveDeg(MechanicalDegrees::new(45.0)));
        assert_eq!(&bytes, b"MD:45.00\n");
    }

    #[test]
    fn encode_set_reverse_serialises_to_fn_one_or_zero() {
        let on = FalconCodec.encode(&Command::SetReverse(true));
        assert_eq!(&on, b"FN:1\n");
        let off = FalconCodec.encode(&Command::SetReverse(false));
        assert_eq!(&off, b"FN:0\n");
    }

    // ---- decode -----------------------------------------------------------

    #[test]
    fn decode_ping_ack_with_trailing_newline() {
        let resp = FalconCodec.decode(b"FR_OK\n").unwrap();
        assert_eq!(resp, FalconResponse::Ack);
    }

    #[test]
    fn decode_full_status_strips_terminator_and_parses_fields() {
        let resp = FalconCodec.decode(b"FR_OK:4332:50.00:0:0:0:0\n").unwrap();
        let status = match resp {
            FalconResponse::Status(s) => s,
            other => panic!("expected Status, got {other:?}"),
        };
        assert_eq!(status.position_steps, Steps(4332));
        assert!((status.position_deg.value() - 50.0).abs() < 1e-9);
        assert!(!status.motion.is_moving);
    }

    #[test]
    fn decode_firmware_version() {
        let resp = FalconCodec.decode(b"FV:1.3\n").unwrap();
        assert_eq!(resp, FalconResponse::FirmwareVersion("1.3".to_string()));
    }

    #[test]
    fn decode_position_deg() {
        let resp = FalconCodec.decode(b"FD:142.30\n").unwrap();
        let v = match resp {
            FalconResponse::PositionDeg(v) => v,
            other => panic!("expected PositionDeg, got {other:?}"),
        };
        assert!((v.value() - 142.30).abs() < 1e-9);
    }

    #[test]
    fn decode_position_steps() {
        let resp = FalconCodec.decode(b"FP:4332\n").unwrap();
        assert_eq!(resp, FalconResponse::PositionSteps(Steps(4332)));
    }

    #[test]
    fn decode_position_steps_accepts_negative_below_home() {
        // Real hardware (firmware 1.5) reports negative steps CCW of the 0°
        // home; the codec must decode them rather than abort the frame.
        let resp = FalconCodec.decode(b"FP:-1784\n").unwrap();
        assert_eq!(resp, FalconResponse::PositionSteps(Steps(-1784)));
    }

    #[test]
    fn decode_full_status_accepts_negative_steps_below_home() {
        let resp = FalconCodec.decode(b"FR_OK:-2838:327.24:1:0:0:0\n").unwrap();
        match resp {
            FalconResponse::Status(s) => assert_eq!(s.position_steps, Steps(-2838)),
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn decode_voltage() {
        let resp = FalconCodec.decode(b"VS:812\n").unwrap();
        assert_eq!(resp, FalconResponse::Voltage(812));
    }

    #[test]
    fn decode_is_running_true_and_false() {
        assert_eq!(
            FalconCodec.decode(b"FR:1\n").unwrap(),
            FalconResponse::IsRunning(true)
        );
        assert_eq!(
            FalconCodec.decode(b"FR:0\n").unwrap(),
            FalconResponse::IsRunning(false)
        );
    }

    #[test]
    fn decode_echo_for_move_deg_is_passed_through_verbatim() {
        let resp = FalconCodec.decode(b"MD:180.00\n").unwrap();
        assert_eq!(resp, FalconResponse::Echo("MD:180.00".to_string()));
    }

    #[test]
    fn decode_echo_for_halt_is_fh_one() {
        let resp = FalconCodec.decode(b"FH:1\n").unwrap();
        assert_eq!(resp, FalconResponse::Echo("FH:1".to_string()));
    }

    #[test]
    fn decode_echo_for_set_reverse() {
        let resp = FalconCodec.decode(b"FN:0\n").unwrap();
        assert_eq!(resp, FalconResponse::Echo("FN:0".to_string()));
    }

    #[test]
    fn decode_invalid_full_status_returns_codec_error() {
        let err = FalconCodec.decode(b"FR_OK:bad\n").unwrap_err();
        assert!(matches!(
            err,
            FalconCodecError::Parse(_) | FalconCodecError::InvalidResponse(_)
        ));
    }

    #[test]
    fn decode_invalid_utf8_returns_utf8_variant() {
        let bad: Vec<u8> = vec![0xFF, 0xFE, 0xFD];
        let err = FalconCodec.decode(&bad).unwrap_err();
        assert!(matches!(err, FalconCodecError::Utf8(_)));
    }

    // ---- matches ----------------------------------------------------------

    #[test]
    fn matches_pairs_typed_commands_with_their_response_variants() {
        let codec = FalconCodec;
        assert!(codec.matches(&Command::Ping, &FalconResponse::Ack));
        assert!(codec.matches(
            &Command::FullStatus,
            &FalconResponse::Status(FalconStatus {
                position_steps: Steps(0),
                position_deg: MechanicalDegrees::new(0.0),
                motion: FalconMotion {
                    is_moving: false,
                    limit_detect: false,
                },
                settings: FalconSettings {
                    do_derotation: false,
                    motor_reverse: false,
                },
            })
        ));
        assert!(codec.matches(
            &Command::FirmwareVersion,
            &FalconResponse::FirmwareVersion("1.3".to_string())
        ));
        assert!(codec.matches(&Command::Voltage, &FalconResponse::Voltage(0)));
    }

    #[test]
    fn matches_pairs_echo_commands_with_echo_variant() {
        let codec = FalconCodec;
        assert!(codec.matches(
            &Command::MoveDeg(MechanicalDegrees::new(180.0)),
            &FalconResponse::Echo("MD:180.00".to_string())
        ));
        assert!(codec.matches(&Command::Halt, &FalconResponse::Echo("FH:1".to_string())));
        assert!(codec.matches(
            &Command::SetReverse(true),
            &FalconResponse::Echo("FN:1".to_string())
        ));
        assert!(codec.matches(
            &Command::DerotationOff,
            &FalconResponse::Echo("DR:0".to_string())
        ));
    }

    #[test]
    fn matches_rejects_cross_shape_pairs() {
        let codec = FalconCodec;
        assert!(!codec.matches(
            &Command::Ping,
            &FalconResponse::FirmwareVersion("1.3".into())
        ));
        assert!(!codec.matches(
            &Command::FullStatus,
            &FalconResponse::Echo("MD:0.00".to_string())
        ));
        assert!(!codec.matches(
            &Command::MoveDeg(MechanicalDegrees::new(0.0)),
            &FalconResponse::IsRunning(true)
        ));
    }

    // ---- FalconCodecError::from_protocol --------------------------------

    #[test]
    fn from_protocol_invalid_response_passes_through() {
        let err = FalconCodecError::from_protocol(FalconRotatorError::InvalidResponse(
            "nope".to_string(),
        ));
        assert!(matches!(err, FalconCodecError::InvalidResponse(s) if s == "nope"));
    }

    #[test]
    fn from_protocol_parse_error_passes_through() {
        let err =
            FalconCodecError::from_protocol(FalconRotatorError::ParseError("bad".to_string()));
        assert!(matches!(err, FalconCodecError::Parse(s) if s == "bad"));
    }

    #[test]
    fn from_protocol_other_variants_flatten_to_invalid_response() {
        let err = FalconCodecError::from_protocol(FalconRotatorError::NotConnected);
        match err {
            FalconCodecError::InvalidResponse(s) => assert!(s.contains("not connected")),
            other => panic!("expected InvalidResponse, got {other:?}"),
        }
    }

    // ---- From<SessionError<FalconCodecError>> for FalconCodecError ------

    #[test]
    fn session_to_codec_error_transport_preserves_inner_variant() {
        // The inner TransportError must survive the flatten so the
        // device-layer mapping can still classify by variant rather than
        // collapse to a stringy Communication error.
        let err: FalconCodecError =
            SessionError::<FalconCodecError>::Transport(TransportError::Eof).into();
        assert!(matches!(
            err,
            FalconCodecError::Transport(TransportError::Eof)
        ));
    }

    #[test]
    fn session_to_codec_error_codec_is_identity() {
        let inner = FalconCodecError::Parse("p".to_string());
        let err: FalconCodecError = SessionError::Codec(inner).into();
        assert!(matches!(err, FalconCodecError::Parse(s) if s == "p"));
    }

    #[test]
    fn session_to_codec_error_skip_exhausted_passes_count() {
        let err: FalconCodecError = SessionError::<FalconCodecError>::SkipExhausted(3).into();
        assert!(matches!(err, FalconCodecError::SkipExhausted(3)));
    }

    // ---- From<SessionError<FalconCodecError>> for FalconRotatorError ---

    #[test]
    fn session_error_transport_open_maps_to_connection_failed() {
        let err: SessionError<FalconCodecError> =
            SessionError::Transport(TransportError::Open(std::io::Error::other("device busy")));
        match FalconRotatorError::from(err) {
            FalconRotatorError::ConnectionFailed(s) => assert!(s.contains("device busy")),
            other => panic!("expected ConnectionFailed, got {other:?}"),
        }
    }

    #[test]
    fn session_error_transport_io_preserves_io_kind() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken");
        let err: SessionError<FalconCodecError> =
            SessionError::Transport(TransportError::Io(io_err));
        match FalconRotatorError::from(err) {
            FalconRotatorError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::BrokenPipe),
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn session_error_transport_timeout_maps_to_timeout() {
        let err: SessionError<FalconCodecError> =
            SessionError::Transport(TransportError::Timeout(std::time::Duration::from_secs(2)));
        match FalconRotatorError::from(err) {
            FalconRotatorError::Timeout(s) => {
                assert!(s.contains("2s") || s.contains("2.0s"));
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn session_error_transport_eof_maps_to_communication() {
        let err: SessionError<FalconCodecError> = SessionError::Transport(TransportError::Eof);
        match FalconRotatorError::from(err) {
            FalconRotatorError::Communication(s) => assert!(s.contains("Connection closed")),
            other => panic!("expected Communication, got {other:?}"),
        }
    }

    #[test]
    fn session_error_transport_framing_maps_to_communication() {
        let err: SessionError<FalconCodecError> =
            SessionError::Transport(TransportError::Framing("too big".to_string()));
        match FalconRotatorError::from(err) {
            FalconRotatorError::Communication(s) => assert!(s.contains("too big")),
            other => panic!("expected Communication, got {other:?}"),
        }
    }

    #[test]
    fn session_error_codec_invalid_response_passes_through() {
        let err: SessionError<FalconCodecError> =
            SessionError::Codec(FalconCodecError::InvalidResponse("nope".to_string()));
        assert!(
            matches!(FalconRotatorError::from(err), FalconRotatorError::InvalidResponse(s) if s == "nope")
        );
    }

    #[test]
    fn session_error_codec_parse_maps_to_parse_error() {
        let err: SessionError<FalconCodecError> =
            SessionError::Codec(FalconCodecError::Parse("nan".to_string()));
        assert!(
            matches!(FalconRotatorError::from(err), FalconRotatorError::ParseError(s) if s == "nan")
        );
    }

    #[test]
    fn session_error_codec_utf8_maps_to_invalid_response() {
        let bad: Vec<u8> = vec![0xFF, 0xFE, 0xFD];
        let utf8_err = std::str::from_utf8(&bad).unwrap_err();
        let err: SessionError<FalconCodecError> =
            SessionError::Codec(FalconCodecError::Utf8(utf8_err));
        assert!(matches!(
            FalconRotatorError::from(err),
            FalconRotatorError::InvalidResponse(_)
        ));
    }

    #[test]
    fn session_error_codec_transport_timeout_routes_to_timeout() {
        // A transport timeout surfaced *through* the handshake hook
        // arrives at the device layer as
        // SessionError::Codec(FalconCodecError::Transport(Timeout(...))).
        // It must map to FalconRotatorError::Timeout — same classification
        // a steady-state timeout (SessionError::Transport(Timeout(...)))
        // would receive — so the ASCOM client doesn't see a generic
        // Communication error for connect-time timeouts.
        let err: SessionError<FalconCodecError> = SessionError::Codec(FalconCodecError::Transport(
            TransportError::Timeout(std::time::Duration::from_secs(2)),
        ));
        match FalconRotatorError::from(err) {
            FalconRotatorError::Timeout(s) => {
                assert!(s.contains("2s") || s.contains("2.0s"));
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn session_error_codec_transport_open_routes_to_connection_failed() {
        let err: SessionError<FalconCodecError> = SessionError::Codec(FalconCodecError::Transport(
            TransportError::Open(std::io::Error::other("device busy")),
        ));
        match FalconRotatorError::from(err) {
            FalconRotatorError::ConnectionFailed(s) => assert!(s.contains("device busy")),
            other => panic!("expected ConnectionFailed, got {other:?}"),
        }
    }

    #[test]
    fn session_error_codec_transport_eof_routes_to_communication() {
        let err: SessionError<FalconCodecError> =
            SessionError::Codec(FalconCodecError::Transport(TransportError::Eof));
        match FalconRotatorError::from(err) {
            FalconRotatorError::Communication(s) => assert!(s.contains("Connection closed")),
            other => panic!("expected Communication, got {other:?}"),
        }
    }

    #[test]
    fn session_error_codec_skip_exhausted_maps_to_communication_with_count() {
        let err: SessionError<FalconCodecError> =
            SessionError::Codec(FalconCodecError::SkipExhausted(7));
        match FalconRotatorError::from(err) {
            FalconRotatorError::Communication(s) => {
                assert!(s.contains("non-matching") && s.contains('7'));
            }
            other => panic!("expected Communication, got {other:?}"),
        }
    }

    #[test]
    fn session_error_skip_exhausted_maps_to_communication() {
        let err: SessionError<FalconCodecError> = SessionError::SkipExhausted(2);
        match FalconRotatorError::from(err) {
            FalconRotatorError::Communication(s) => {
                assert!(s.contains("non-matching") && s.contains('2'));
            }
            other => panic!("expected Communication, got {other:?}"),
        }
    }
}

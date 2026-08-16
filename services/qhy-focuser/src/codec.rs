//! Frame codec for the QHY Q-Focuser JSON protocol.
//!
//! The [`QhyCodec`] is a zero-sized adapter that plugs into
//! [`rusty_photon_shared_transport::SharedTransport`]. It owns the
//! bytes↔typed translation for both encode and decode, plus a
//! [`matches`](Codec::matches) predicate that verifies a decoded frame is
//! the response to the request that produced it.
//!
//! Wire shape (Q-Focuser):
//!
//! * Commands are bare JSON objects (no terminator) sent over USB-CDC.
//! * Responses are bare JSON objects terminated by `}` (no LF, no
//!   trailing whitespace) — the `SerialFrameTransport` is configured with
//!   `b'}'` as the framing terminator so each read returns exactly one
//!   reply (including the closing brace).
//! * Every command and reply carries a `cmd_id` / `idx` field. The codec
//!   uses that to match reply→command via [`Codec::matches`]; the device
//!   intermittently emits unsolicited position frames (idx 5) mid-move,
//!   so [`Codec::max_skip`] is set to 5 so the request layer can discard
//!   up to five stale frames before erroring.

use std::str::Utf8Error;

use rusty_photon_shared_transport::{Codec, SessionError, TransportError};
use serde_json::Value;
use thiserror::Error;

use crate::error::QhyFocuserError;
use crate::protocol::{
    extract_idx, parse_position_value, parse_temperature_value, parse_version_value, Command,
    PositionResponse, TemperatureResponse, VersionResponse,
};

/// Decoded response frame from the device.
///
/// `Ack` represents any reply whose body the driver doesn't need to
/// inspect — set/move/abort commands ack with just an `idx`, and the
/// codec's [`matches`](Codec::matches) predicate validates that the
/// `idx` actually corresponds to the command that was sent.
#[derive(Debug, Clone)]
pub enum QhyResponse {
    Version(VersionResponse),
    Position(PositionResponse),
    Temperature(TemperatureResponse),
    Ack { idx: u8 },
}

impl QhyResponse {
    /// `idx` carried by this response — the protocol's reply-to-command
    /// correlator.
    #[must_use]
    pub const fn idx(&self) -> u8 {
        match self {
            Self::Version(_) => 1,
            Self::Position(_) => 5,
            Self::Temperature(_) => 4,
            Self::Ack { idx } => *idx,
        }
    }
}

/// Codec-side error type.
///
/// Carries enough variants to flatten a full [`SessionError<QhyCodecError>`]
/// in handshake / poll-loop contexts so `?` works without losing
/// information that the device-layer
/// `From<SessionError<…>> for QhyFocuserError` then re-expands into the
/// right [`QhyFocuserError`] variant.
///
/// `Transport` carries the underlying [`TransportError`] structurally
/// rather than as a string so a transport-level failure surfaced
/// *through* the handshake hook (which returns `Result<_, QhyCodecError>`)
/// can still be classified as `Open` / `Io` / `Timeout` / `Eof` /
/// `Framing` by the device layer instead of collapsing to a generic
/// `Communication` error.
#[derive(Debug, Error)]
pub enum QhyCodecError {
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

impl QhyCodecError {
    fn from_protocol(err: QhyFocuserError) -> Self {
        match err {
            QhyFocuserError::InvalidResponse(s) => Self::InvalidResponse(s),
            QhyFocuserError::ParseError(s) => Self::Parse(s),
            other => Self::InvalidResponse(other.to_string()),
        }
    }
}

impl From<SessionError<Self>> for QhyCodecError {
    fn from(err: SessionError<Self>) -> Self {
        match err {
            SessionError::Transport(t) => Self::Transport(t),
            SessionError::Codec(c) => c,
            SessionError::SkipExhausted(n) => Self::SkipExhausted(n),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct QhyCodec;

impl Codec for QhyCodec {
    type Command = Command;
    type Response = QhyResponse;
    type Error = QhyCodecError;

    fn encode(&self, cmd: &Self::Command) -> Vec<u8> {
        // Q-Focuser JSON commands are sent without a trailing terminator;
        // the device parses balanced braces on input. The framer's
        // `recv_frame` terminator is for *responses*, so we don't append
        // anything here.
        cmd.to_json_string().into_bytes()
    }

    fn decode(&self, bytes: &[u8]) -> Result<Self::Response, Self::Error> {
        let text = std::str::from_utf8(bytes)?.trim();
        // The device's responses are flat JSON objects terminated by `}`;
        // `SerialFrameTransport` includes the terminator. Trim leading
        // junk (matches the legacy `TokioSerialReader` defensive
        // behaviour) before parsing.
        let json = text
            .find('{')
            .and_then(|start| text.get(start..))
            .ok_or_else(|| {
                QhyCodecError::InvalidResponse(format!("no `{{` found in frame: {text:?}"))
            })?;

        let value: Value = serde_json::from_str(json)
            .map_err(|e| QhyCodecError::Parse(format!("invalid JSON: {e}")))?;
        let idx = extract_idx(&value).map_err(QhyCodecError::from_protocol)?;

        match idx {
            1 => Ok(QhyResponse::Version(parse_version_value(&value))),
            4 => Ok(QhyResponse::Temperature(
                parse_temperature_value(&value).map_err(QhyCodecError::from_protocol)?,
            )),
            5 => Ok(QhyResponse::Position(
                parse_position_value(&value).map_err(QhyCodecError::from_protocol)?,
            )),
            // Set / move / abort / sync commands ack with just `idx`.
            2 | 3 | 6 | 7 | 11 | 13 | 16 | 19 => Ok(QhyResponse::Ack { idx }),
            other => Err(QhyCodecError::InvalidResponse(format!(
                "unknown response idx {other}"
            ))),
        }
    }

    fn matches(&self, cmd: &Self::Command, resp: &Self::Response) -> bool {
        cmd.cmd_id() == resp.idx()
    }

    fn max_skip(&self) -> usize {
        // The device emits unsolicited position (idx 5) frames during
        // movement; the legacy driver allowed up to 5 stale-frame
        // discards before erroring. Preserve that.
        5
    }
}

impl From<SessionError<QhyCodecError>> for QhyFocuserError {
    fn from(err: SessionError<QhyCodecError>) -> Self {
        match err {
            // Both alternatives route through `From<TransportError> for
            // QhyFocuserError` in error.rs so a timeout that surfaces
            // *through* the handshake hook (codec alternative) gets the
            // same classification as one that surfaces on a steady-state
            // request (transport alternative).
            SessionError::Transport(t) | SessionError::Codec(QhyCodecError::Transport(t)) => {
                t.into()
            }
            SessionError::Codec(QhyCodecError::InvalidResponse(s)) => Self::InvalidResponse(s),
            SessionError::Codec(QhyCodecError::Parse(s)) => Self::ParseError(s),
            SessionError::Codec(c @ QhyCodecError::Utf8(_)) => Self::InvalidResponse(c.to_string()),
            SessionError::Codec(QhyCodecError::SkipExhausted(n)) => Self::Communication(format!(
                "device returned non-matching response ({n} frame(s) read)"
            )),
            SessionError::SkipExhausted(n) => Self::Communication(format!(
                "device returned non-matching response ({n} frame{s} read)",
                s = if n == 1 { "" } else { "s" }
            )),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use rusty_photon_shared_transport::TransportError;

    // ============================================================================
    // Codec::encode
    // ============================================================================

    #[test]
    fn encode_emits_bare_json_object_without_terminator() {
        let bytes = QhyCodec.encode(&Command::GetPosition);
        let text = std::str::from_utf8(&bytes).unwrap();
        let value: Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["cmd_id"], 5);
        // No trailing terminator — responses use `}`, commands don't.
        assert!(!text.ends_with('\n'));
    }

    #[test]
    fn encode_absolute_move_carries_target() {
        let bytes = QhyCodec.encode(&Command::AbsoluteMove { position: 12345 });
        let value: Value = serde_json::from_str(std::str::from_utf8(&bytes).unwrap()).unwrap();
        assert_eq!(value["cmd_id"], 6);
        assert_eq!(value["tar"], 12345);
    }

    // ============================================================================
    // Codec::decode
    // ============================================================================

    #[test]
    fn decode_version_frame() {
        let frame = br#"{"idx": 1, "firmware_version": "2.1.0", "board_version": "1.0"}"#;
        match QhyCodec.decode(frame).unwrap() {
            QhyResponse::Version(v) => {
                assert_eq!(v.firmware_version, "2.1.0");
                assert_eq!(v.board_version, "1.0");
            }
            other => panic!("expected Version, got {other:?}"),
        }
    }

    #[test]
    fn decode_position_frame() {
        let frame = br#"{"idx": 5, "pos": 32000}"#;
        match QhyCodec.decode(frame).unwrap() {
            QhyResponse::Position(p) => assert_eq!(p.position, 32000),
            other => panic!("expected Position, got {other:?}"),
        }
    }

    #[test]
    fn decode_temperature_frame_scales_raw_values() {
        let frame = br#"{"idx": 4, "o_t": 25000, "c_t": 30000, "c_r": 125}"#;
        match QhyCodec.decode(frame).unwrap() {
            QhyResponse::Temperature(t) => {
                assert!((t.outer_temp - 25.0).abs() < 1e-6);
                assert!((t.chip_temp - 30.0).abs() < 1e-6);
                assert!((t.voltage - 12.5).abs() < 1e-6);
            }
            other => panic!("expected Temperature, got {other:?}"),
        }
    }

    #[test]
    fn decode_set_command_response_yields_ack() {
        let frame = br#"{"idx": 13}"#;
        match QhyCodec.decode(frame).unwrap() {
            QhyResponse::Ack { idx } => assert_eq!(idx, 13),
            other => panic!("expected Ack, got {other:?}"),
        }
    }

    #[test]
    fn decode_unknown_idx_errors_as_invalid_response() {
        let frame = br#"{"idx": 99}"#;
        let err = QhyCodec.decode(frame).unwrap_err();
        assert!(matches!(err, QhyCodecError::InvalidResponse(s) if s.contains("99")));
    }

    #[test]
    fn decode_missing_idx_errors() {
        let frame = br#"{"pos": 1000}"#;
        let err = QhyCodec.decode(frame).unwrap_err();
        assert!(matches!(err, QhyCodecError::InvalidResponse(_)));
    }

    #[test]
    fn decode_invalid_json_after_opening_brace_errors_as_parse() {
        // `find('{')` succeeds, so we reach serde_json::from_str — which
        // then errors because the rest isn't a valid object.
        let frame = b"{not json}";
        let err = QhyCodec.decode(frame).unwrap_err();
        assert!(matches!(err, QhyCodecError::Parse(_)));
    }

    #[test]
    fn decode_trims_leading_junk_before_opening_brace() {
        // Matches the legacy defensive behaviour in TokioSerialReader,
        // preserved here so leftover bytes from a partial prior read
        // don't break decode.
        let frame = b"junk{\"idx\": 5, \"pos\": 42}";
        match QhyCodec.decode(frame).unwrap() {
            QhyResponse::Position(p) => assert_eq!(p.position, 42),
            other => panic!("expected Position, got {other:?}"),
        }
    }

    #[test]
    fn decode_frame_without_opening_brace_errors() {
        let err = QhyCodec.decode(b"no brace at all").unwrap_err();
        assert!(matches!(err, QhyCodecError::InvalidResponse(_)));
    }

    // ============================================================================
    // Codec::matches
    // ============================================================================

    #[test]
    fn matches_returns_true_only_when_cmd_id_equals_idx() {
        let cdc = QhyCodec;
        assert!(cdc.matches(
            &Command::GetPosition,
            &QhyResponse::Position(PositionResponse { position: 0 })
        ));
        assert!(!cdc.matches(
            &Command::GetVersion,
            &QhyResponse::Position(PositionResponse { position: 0 })
        ));
        assert!(cdc.matches(
            &Command::AbsoluteMove { position: 100 },
            &QhyResponse::Ack { idx: 6 }
        ));
        assert!(!cdc.matches(
            &Command::AbsoluteMove { position: 100 },
            &QhyResponse::Ack { idx: 13 }
        ));
    }

    #[test]
    fn max_skip_allows_five_stale_frames() {
        assert_eq!(QhyCodec.max_skip(), 5);
    }

    // ============================================================================
    // QhyCodecError::from_protocol
    // ============================================================================

    #[test]
    fn from_protocol_invalid_response_passes_through() {
        let err = QhyCodecError::from_protocol(QhyFocuserError::InvalidResponse("oops".into()));
        assert!(matches!(err, QhyCodecError::InvalidResponse(s) if s == "oops"));
    }

    #[test]
    fn from_protocol_parse_error_passes_through() {
        let err = QhyCodecError::from_protocol(QhyFocuserError::ParseError("bad".into()));
        assert!(matches!(err, QhyCodecError::Parse(s) if s == "bad"));
    }

    #[test]
    fn from_protocol_other_variants_flatten_to_invalid_response() {
        let err = QhyCodecError::from_protocol(QhyFocuserError::NotConnected);
        assert!(matches!(err, QhyCodecError::InvalidResponse(s) if s.contains("not connected")));
    }

    // ============================================================================
    // From<SessionError<QhyCodecError>> for QhyCodecError
    // ============================================================================

    #[test]
    fn session_to_codec_error_transport_preserves_inner_variant() {
        // The inner TransportError must survive the flatten so the
        // device-layer mapping can still classify by variant rather than
        // collapse to a stringy Communication error.
        let err: QhyCodecError =
            SessionError::<QhyCodecError>::Transport(TransportError::Eof).into();
        assert!(matches!(err, QhyCodecError::Transport(TransportError::Eof)));
    }

    #[test]
    fn session_to_codec_error_codec_is_identity() {
        let err: QhyCodecError = SessionError::Codec(QhyCodecError::Parse("p".into())).into();
        assert!(matches!(err, QhyCodecError::Parse(s) if s == "p"));
    }

    #[test]
    fn session_to_codec_error_skip_exhausted_passes_count() {
        let err: QhyCodecError = SessionError::<QhyCodecError>::SkipExhausted(3).into();
        assert!(matches!(err, QhyCodecError::SkipExhausted(3)));
    }

    // ============================================================================
    // From<SessionError<QhyCodecError>> for QhyFocuserError
    // ============================================================================

    #[test]
    fn session_error_transport_open_maps_to_connection_failed() {
        let err: SessionError<QhyCodecError> =
            SessionError::Transport(TransportError::Open(std::io::Error::other("busy")));
        match QhyFocuserError::from(err) {
            QhyFocuserError::ConnectionFailed(s) => assert!(s.contains("busy")),
            other => panic!("expected ConnectionFailed, got {other:?}"),
        }
    }

    #[test]
    fn session_error_transport_io_preserves_io_kind() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken");
        let err: SessionError<QhyCodecError> = SessionError::Transport(TransportError::Io(io_err));
        match QhyFocuserError::from(err) {
            QhyFocuserError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::BrokenPipe),
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn session_error_transport_timeout_maps_to_timeout() {
        let err: SessionError<QhyCodecError> =
            SessionError::Transport(TransportError::Timeout(std::time::Duration::from_secs(2)));
        match QhyFocuserError::from(err) {
            QhyFocuserError::Timeout(s) => assert!(s.contains('2')),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn session_error_transport_eof_maps_to_communication() {
        let err: SessionError<QhyCodecError> = SessionError::Transport(TransportError::Eof);
        assert!(matches!(
            QhyFocuserError::from(err),
            QhyFocuserError::Communication(s) if s.contains("Connection closed")
        ));
    }

    #[test]
    fn session_error_transport_framing_maps_to_communication() {
        let err: SessionError<QhyCodecError> =
            SessionError::Transport(TransportError::Framing("too big".into()));
        assert!(matches!(
            QhyFocuserError::from(err),
            QhyFocuserError::Communication(s) if s.contains("too big")
        ));
    }

    #[test]
    fn session_error_codec_parse_maps_to_parse_error() {
        let err: SessionError<QhyCodecError> =
            SessionError::Codec(QhyCodecError::Parse("p".into()));
        assert!(matches!(QhyFocuserError::from(err), QhyFocuserError::ParseError(s) if s == "p"));
    }

    #[test]
    fn session_error_codec_invalid_response_maps_to_invalid_response() {
        let err: SessionError<QhyCodecError> =
            SessionError::Codec(QhyCodecError::InvalidResponse("nope".into()));
        assert!(matches!(
            QhyFocuserError::from(err),
            QhyFocuserError::InvalidResponse(s) if s == "nope"
        ));
    }

    #[test]
    fn session_error_codec_utf8_maps_to_invalid_response() {
        let bad: Vec<u8> = vec![0xFF, 0xFE, 0xFD];
        let utf8_err = std::str::from_utf8(&bad).unwrap_err();
        let err: SessionError<QhyCodecError> = SessionError::Codec(QhyCodecError::Utf8(utf8_err));
        assert!(matches!(
            QhyFocuserError::from(err),
            QhyFocuserError::InvalidResponse(_)
        ));
    }

    #[test]
    fn session_error_codec_transport_timeout_routes_to_timeout() {
        // A transport timeout surfaced *through* the handshake hook
        // arrives at the device layer as
        // SessionError::Codec(QhyCodecError::Transport(Timeout(...))).
        // It must map to QhyFocuserError::Timeout — same classification
        // a steady-state timeout (SessionError::Transport(Timeout(...)))
        // would receive — so the ASCOM client doesn't see a generic
        // Communication error for connect-time timeouts.
        let err: SessionError<QhyCodecError> = SessionError::Codec(QhyCodecError::Transport(
            TransportError::Timeout(std::time::Duration::from_secs(2)),
        ));
        match QhyFocuserError::from(err) {
            QhyFocuserError::Timeout(s) => assert!(s.contains('2')),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn session_error_codec_transport_open_routes_to_connection_failed() {
        let err: SessionError<QhyCodecError> = SessionError::Codec(QhyCodecError::Transport(
            TransportError::Open(std::io::Error::other("busy")),
        ));
        match QhyFocuserError::from(err) {
            QhyFocuserError::ConnectionFailed(s) => assert!(s.contains("busy")),
            other => panic!("expected ConnectionFailed, got {other:?}"),
        }
    }

    #[test]
    fn session_error_codec_transport_eof_routes_to_communication() {
        let err: SessionError<QhyCodecError> =
            SessionError::Codec(QhyCodecError::Transport(TransportError::Eof));
        assert!(matches!(
            QhyFocuserError::from(err),
            QhyFocuserError::Communication(s) if s.contains("Connection closed")
        ));
    }

    #[test]
    fn session_error_codec_skip_exhausted_maps_to_communication_with_count() {
        let err: SessionError<QhyCodecError> = SessionError::Codec(QhyCodecError::SkipExhausted(7));
        assert!(matches!(
            QhyFocuserError::from(err),
            QhyFocuserError::Communication(s) if s.contains("non-matching") && s.contains('7')
        ));
    }

    #[test]
    fn session_error_skip_exhausted_pluralises_correctly() {
        let one: SessionError<QhyCodecError> = SessionError::SkipExhausted(1);
        let many: SessionError<QhyCodecError> = SessionError::SkipExhausted(2);
        let one_msg = QhyFocuserError::from(one).to_string();
        let many_msg = QhyFocuserError::from(many).to_string();
        assert!(one_msg.contains("1 frame "));
        assert!(many_msg.contains("2 frames "));
    }

    #[test]
    fn qhy_response_idx_returns_protocol_idx_per_variant() {
        assert_eq!(
            QhyResponse::Version(VersionResponse {
                firmware_version: String::new(),
                board_version: String::new(),
            })
            .idx(),
            1
        );
        assert_eq!(
            QhyResponse::Position(PositionResponse { position: 0 }).idx(),
            5
        );
        assert_eq!(
            QhyResponse::Temperature(TemperatureResponse {
                outer_temp: 0.0,
                chip_temp: 0.0,
                voltage: 0.0,
            })
            .idx(),
            4
        );
        assert_eq!(QhyResponse::Ack { idx: 11 }.idx(), 11);
    }
}

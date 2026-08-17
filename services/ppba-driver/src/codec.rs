//! Frame codec for the PPBA serial protocol.
//!
//! The [`PpbaCodec`] is a zero-sized adapter that plugs into
//! [`rusty_photon_shared_transport::SharedTransport`]. It owns the bytes↔typed
//! translation for both encode and decode, plus a [`matches`](Codec::matches)
//! predicate that verifies a decoded frame is the response to the request
//! that produced it.
//!
//! Wire shape (PPBA Gen2):
//!
//! * Commands are short ASCII strings terminated by `\n`.
//! * Replies are one line per request, also `\n`-terminated.
//! * Three reply shapes: `PPBA_OK` (ping), `PPBA:...` (status), `PS:...`
//!   (power stats), or an echo of the command string (set / version).

use std::str::Utf8Error;

use rusty_photon_shared_transport::{Codec, SessionError, TransportError};
use thiserror::Error;

use crate::error::PpbaError;
use crate::protocol::{PpbaCommand, PpbaPowerStats, PpbaStatus};

/// Decoded response frame from the device.
///
/// `Echo` carries the raw trimmed reply for set commands and firmware
/// version reads — the codec's [`matches`](Codec::matches) predicate
/// validates that the echo actually corresponds to the command sent.
#[derive(Debug, Clone)]
pub enum PpbaResponse {
    PingOk,
    Status(PpbaStatus),
    PowerStats(PpbaPowerStats),
    Echo(String),
}

/// Codec-side error type.
///
/// Carries enough variants to flatten a full [`SessionError<PpbaCodecError>`]
/// in handshake / poll-loop contexts so `?` works without losing
/// information that the device-layer `From<SessionError<…>> for PpbaError`
/// then re-expands into the right `PpbaError` variant.
///
/// `Transport` carries the underlying [`TransportError`] structurally
/// rather than as a string so a transport-level failure surfaced
/// *through* the handshake hook (which returns `Result<_, PpbaCodecError>`)
/// can still be classified as `Open` / `Io` / `Timeout` / `Eof` /
/// `Framing` by the device layer instead of collapsing to a generic
/// `Communication` error.
#[derive(Debug, Error)]
pub enum PpbaCodecError {
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

impl PpbaCodecError {
    fn from_protocol(err: PpbaError) -> Self {
        match err {
            PpbaError::InvalidResponse(s) => Self::InvalidResponse(s),
            PpbaError::ParseError(s) => Self::Parse(s),
            other => Self::InvalidResponse(other.to_string()),
        }
    }
}

impl From<SessionError<Self>> for PpbaCodecError {
    fn from(err: SessionError<Self>) -> Self {
        match err {
            SessionError::Transport(t) => Self::Transport(t),
            SessionError::Codec(c) => c,
            SessionError::SkipExhausted(n) => Self::SkipExhausted(n),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PpbaCodec;

impl Codec for PpbaCodec {
    type Command = PpbaCommand;
    type Response = PpbaResponse;
    type Error = PpbaCodecError;

    fn encode(&self, cmd: &Self::Command) -> Vec<u8> {
        let mut bytes = cmd.to_command_string().into_bytes();
        bytes.push(b'\n');
        bytes
    }

    fn decode(&self, bytes: &[u8]) -> Result<Self::Response, Self::Error> {
        let text = std::str::from_utf8(bytes)?.trim();
        if text == "PPBA_OK" {
            return Ok(PpbaResponse::PingOk);
        }
        if text.starts_with("PPBA:") {
            return text
                .parse::<PpbaStatus>()
                .map(PpbaResponse::Status)
                .map_err(PpbaCodecError::from_protocol);
        }
        if text.starts_with("PS:") {
            return text
                .parse::<PpbaPowerStats>()
                .map(PpbaResponse::PowerStats)
                .map_err(PpbaCodecError::from_protocol);
        }
        Ok(PpbaResponse::Echo(text.to_string()))
    }

    fn matches(&self, cmd: &Self::Command, resp: &Self::Response) -> bool {
        match (cmd, resp) {
            // The firmware-version alternative accepts any echo body: the
            // wire protocol gives `n.n.n` and the existing driver never
            // validated it — keep that here.
            (PpbaCommand::Ping, PpbaResponse::PingOk)
            | (PpbaCommand::Status, PpbaResponse::Status(_))
            | (PpbaCommand::PowerStats, PpbaResponse::PowerStats(_))
            | (PpbaCommand::FirmwareVersion, PpbaResponse::Echo(_)) => true,
            // Set commands echo their command string. PPBA Gen2 hardware
            // appends nothing extra, but the legacy code accepted
            // `starts_with` so we match that lenience exactly.
            (
                PpbaCommand::SetQuad12V(_)
                | PpbaCommand::SetAdjustable(_)
                | PpbaCommand::SetDewA(_)
                | PpbaCommand::SetDewB(_)
                | PpbaCommand::SetUsbHub(_)
                | PpbaCommand::SetAutoDew(_),
                PpbaResponse::Echo(echo),
            ) => echo.starts_with(&cmd.to_command_string()),
            _ => false,
        }
    }
}

impl From<SessionError<PpbaCodecError>> for PpbaError {
    fn from(err: SessionError<PpbaCodecError>) -> Self {
        match err {
            // Both alternatives route through `From<TransportError> for
            // PpbaError` in error.rs so a timeout that surfaces *through*
            // the handshake hook (codec alternative) gets the same
            // classification as one that surfaces on a steady-state
            // request (transport alternative).
            SessionError::Transport(t) | SessionError::Codec(PpbaCodecError::Transport(t)) => {
                t.into()
            }
            SessionError::Codec(PpbaCodecError::InvalidResponse(s)) => Self::InvalidResponse(s),
            SessionError::Codec(PpbaCodecError::Parse(s)) => Self::ParseError(s),
            SessionError::Codec(c @ PpbaCodecError::Utf8(_)) => {
                Self::InvalidResponse(c.to_string())
            }
            SessionError::Codec(PpbaCodecError::SkipExhausted(n)) => Self::Communication(format!(
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
    use crate::protocol::PwmDuty;
    use rusty_photon_shared_transport::TransportError;

    #[test]
    fn encode_appends_newline_terminator() {
        let bytes = PpbaCodec.encode(&PpbaCommand::Ping);
        assert_eq!(&bytes, b"P#\n");
    }

    #[test]
    fn encode_set_command_includes_argument() {
        let bytes = PpbaCodec.encode(&PpbaCommand::SetDewA(PwmDuty(200)));
        assert_eq!(&bytes, b"P3:200\n");
    }

    #[test]
    fn decode_ping_response() {
        let resp = PpbaCodec.decode(b"PPBA_OK\n").unwrap();
        assert!(matches!(resp, PpbaResponse::PingOk));
    }

    #[test]
    fn decode_status_response_strips_terminator_and_parses() {
        let frame = b"PPBA:12.5:3.2:25.0:60:15.5:1:0:128:64:0:0:0\n";
        let resp = PpbaCodec.decode(frame).unwrap();
        let status = match resp {
            PpbaResponse::Status(s) => s,
            other => panic!("expected Status, got {other:?}"),
        };
        assert!((status.voltage - 12.5).abs() < f64::EPSILON);
        assert!(status.switches.quad_12v);
        assert_eq!(status.dew_a, 128);
    }

    #[test]
    fn decode_power_stats_response() {
        let frame = b"PS:2.5:10.5:126.0:3600000\n";
        let resp = PpbaCodec.decode(frame).unwrap();
        let stats = match resp {
            PpbaResponse::PowerStats(p) => p,
            other => panic!("expected PowerStats, got {other:?}"),
        };
        assert!((stats.average_amps - 2.5).abs() < f64::EPSILON);
        assert!((stats.uptime_hours() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn decode_set_command_echo_becomes_echo_variant() {
        let resp = PpbaCodec.decode(b"P1:1\n").unwrap();
        match resp {
            PpbaResponse::Echo(s) => assert_eq!(s, "P1:1"),
            other => panic!("expected Echo, got {other:?}"),
        }
    }

    #[test]
    fn decode_invalid_status_returns_codec_error() {
        let err = PpbaCodec.decode(b"PPBA:bad\n").unwrap_err();
        assert!(matches!(
            err,
            PpbaCodecError::Parse(_) | PpbaCodecError::InvalidResponse(_)
        ));
    }

    #[test]
    fn matches_pairs_command_classes_with_response_variants() {
        let cdc = PpbaCodec;
        assert!(cdc.matches(&PpbaCommand::Ping, &PpbaResponse::PingOk));
        assert!(cdc.matches(
            &PpbaCommand::Status,
            &PpbaResponse::Status(PpbaStatus::default())
        ));
        assert!(cdc.matches(
            &PpbaCommand::PowerStats,
            &PpbaResponse::PowerStats(PpbaPowerStats::default())
        ));
        assert!(!cdc.matches(&PpbaCommand::Ping, &PpbaResponse::Echo("WRONG".to_string())));
        assert!(!cdc.matches(
            &PpbaCommand::Status,
            &PpbaResponse::Echo("PPBA_OK".to_string())
        ));
    }

    #[test]
    fn matches_set_command_echo_must_start_with_command_string() {
        let cdc = PpbaCodec;
        let cmd = PpbaCommand::SetQuad12V(true);
        assert!(cdc.matches(&cmd, &PpbaResponse::Echo("P1:1".to_string())));
        assert!(!cdc.matches(&cmd, &PpbaResponse::Echo("WRONG".to_string())));
    }

    #[test]
    fn session_error_codec_invalid_response_flattens() {
        let err: SessionError<PpbaCodecError> =
            SessionError::Codec(PpbaCodecError::InvalidResponse("nope".to_string()));
        let ppba = PpbaError::from(err);
        assert!(matches!(ppba, PpbaError::InvalidResponse(s) if s == "nope"));
    }

    #[test]
    fn session_error_skip_exhausted_maps_to_communication() {
        let err: SessionError<PpbaCodecError> = SessionError::SkipExhausted(1);
        let ppba = PpbaError::from(err);
        assert!(matches!(ppba, PpbaError::Communication(_)));
    }

    // ============================================================================
    // PpbaCodecError::from_protocol: tightens PpbaError variants into the codec
    // error type for use inside Codec::decode's parser-error branches.
    // ============================================================================

    #[test]
    fn from_protocol_invalid_response_passes_through() {
        let err = PpbaCodecError::from_protocol(PpbaError::InvalidResponse("nope".to_string()));
        assert!(matches!(err, PpbaCodecError::InvalidResponse(s) if s == "nope"));
    }

    #[test]
    fn from_protocol_parse_error_passes_through() {
        let err = PpbaCodecError::from_protocol(PpbaError::ParseError("bad".to_string()));
        assert!(matches!(err, PpbaCodecError::Parse(s) if s == "bad"));
    }

    #[test]
    fn from_protocol_other_variants_flatten_to_invalid_response() {
        let err = PpbaCodecError::from_protocol(PpbaError::NotConnected);
        match err {
            PpbaCodecError::InvalidResponse(s) => assert!(s.contains("not connected")),
            other => panic!("expected InvalidResponse, got {other:?}"),
        }
    }

    // ============================================================================
    // From<SessionError<PpbaCodecError>> for PpbaCodecError: used in handshake
    // and poll-loop contexts so `?` flattens transport-side failures into the
    // codec error type without losing structural information.
    // ============================================================================

    #[test]
    fn session_to_codec_error_transport_preserves_inner_variant() {
        // The inner TransportError must survive the flatten so the
        // device-layer mapping can classify by variant rather than
        // collapse to a stringy Communication error.
        let err: PpbaCodecError =
            SessionError::<PpbaCodecError>::Transport(TransportError::Eof).into();
        assert!(matches!(
            err,
            PpbaCodecError::Transport(TransportError::Eof)
        ));
    }

    #[test]
    fn session_to_codec_error_codec_is_identity() {
        let inner = PpbaCodecError::Parse("p".to_string());
        let err: PpbaCodecError = SessionError::Codec(inner).into();
        assert!(matches!(err, PpbaCodecError::Parse(s) if s == "p"));
    }

    #[test]
    fn session_to_codec_error_skip_exhausted_passes_count() {
        let err: PpbaCodecError = SessionError::<PpbaCodecError>::SkipExhausted(3).into();
        assert!(matches!(err, PpbaCodecError::SkipExhausted(3)));
    }

    // ============================================================================
    // From<SessionError<PpbaCodecError>> for PpbaError: the device-layer mapping
    // that decides which ASCOMErrorCode each failure ultimately surfaces as.
    // ============================================================================

    #[test]
    fn session_error_transport_open_maps_to_connection_failed() {
        let err: SessionError<PpbaCodecError> =
            SessionError::Transport(TransportError::Open(std::io::Error::other("device busy")));
        match PpbaError::from(err) {
            PpbaError::ConnectionFailed(s) => assert!(s.contains("device busy")),
            other => panic!("expected ConnectionFailed, got {other:?}"),
        }
    }

    #[test]
    fn session_error_transport_io_preserves_io_kind() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken");
        let err: SessionError<PpbaCodecError> = SessionError::Transport(TransportError::Io(io_err));
        match PpbaError::from(err) {
            PpbaError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::BrokenPipe),
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn session_error_transport_timeout_maps_to_timeout() {
        let err: SessionError<PpbaCodecError> =
            SessionError::Transport(TransportError::Timeout(std::time::Duration::from_secs(2)));
        match PpbaError::from(err) {
            PpbaError::Timeout(s) => assert!(s.contains("2s") || s.contains("2.0s")),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn session_error_transport_eof_maps_to_communication() {
        let err: SessionError<PpbaCodecError> = SessionError::Transport(TransportError::Eof);
        match PpbaError::from(err) {
            PpbaError::Communication(s) => assert!(s.contains("Connection closed")),
            other => panic!("expected Communication, got {other:?}"),
        }
    }

    #[test]
    fn session_error_transport_framing_maps_to_communication() {
        let err: SessionError<PpbaCodecError> =
            SessionError::Transport(TransportError::Framing("too big".to_string()));
        match PpbaError::from(err) {
            PpbaError::Communication(s) => assert!(s.contains("too big")),
            other => panic!("expected Communication, got {other:?}"),
        }
    }

    #[test]
    fn session_error_codec_parse_maps_to_parse_error() {
        let err: SessionError<PpbaCodecError> =
            SessionError::Codec(PpbaCodecError::Parse("nan".to_string()));
        assert!(matches!(PpbaError::from(err), PpbaError::ParseError(s) if s == "nan"));
    }

    #[test]
    fn session_error_codec_utf8_maps_to_invalid_response() {
        // Build a real Utf8Error by decoding a non-literal byte slice; using
        // a literal trips the `invalid_from_utf8` lint.
        let bad: Vec<u8> = vec![0xFF, 0xFE, 0xFD];
        let utf8_err = std::str::from_utf8(&bad).unwrap_err();
        let err: SessionError<PpbaCodecError> = SessionError::Codec(PpbaCodecError::Utf8(utf8_err));
        assert!(matches!(
            PpbaError::from(err),
            PpbaError::InvalidResponse(_)
        ));
    }

    #[test]
    fn session_error_codec_transport_timeout_routes_to_timeout() {
        // A transport timeout surfaced *through* the handshake hook
        // arrives at the device layer as
        // SessionError::Codec(PpbaCodecError::Transport(Timeout(...))).
        // It must map to PpbaError::Timeout — same classification a
        // steady-state timeout (SessionError::Transport(Timeout(...)))
        // would receive — so the ASCOM client doesn't see a generic
        // Communication error for connect-time timeouts.
        let err: SessionError<PpbaCodecError> = SessionError::Codec(PpbaCodecError::Transport(
            TransportError::Timeout(std::time::Duration::from_secs(2)),
        ));
        match PpbaError::from(err) {
            PpbaError::Timeout(s) => assert!(s.contains('2')),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn session_error_codec_transport_open_routes_to_connection_failed() {
        let err: SessionError<PpbaCodecError> = SessionError::Codec(PpbaCodecError::Transport(
            TransportError::Open(std::io::Error::other("device busy")),
        ));
        match PpbaError::from(err) {
            PpbaError::ConnectionFailed(s) => assert!(s.contains("device busy")),
            other => panic!("expected ConnectionFailed, got {other:?}"),
        }
    }

    #[test]
    fn session_error_codec_transport_eof_routes_to_communication() {
        let err: SessionError<PpbaCodecError> =
            SessionError::Codec(PpbaCodecError::Transport(TransportError::Eof));
        assert!(matches!(
            PpbaError::from(err),
            PpbaError::Communication(s) if s.contains("Connection closed")
        ));
    }

    #[test]
    fn session_error_codec_skip_exhausted_maps_to_communication_with_count() {
        let err: SessionError<PpbaCodecError> =
            SessionError::Codec(PpbaCodecError::SkipExhausted(7));
        match PpbaError::from(err) {
            PpbaError::Communication(s) => {
                assert!(s.contains("non-matching") && s.contains('7'));
            }
            other => panic!("expected Communication, got {other:?}"),
        }
    }

    // ============================================================================
    // Codec::matches: FirmwareVersion echo accept.
    // ============================================================================

    #[test]
    fn matches_firmware_version_accepts_any_echo() {
        let cdc = PpbaCodec;
        assert!(cdc.matches(
            &PpbaCommand::FirmwareVersion,
            &PpbaResponse::Echo("1.2.3".to_string())
        ));
    }
}

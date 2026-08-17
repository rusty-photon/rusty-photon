//! Frame codec for the Pegasus Scops OAG serial protocol.
//!
//! Plugs into [`rusty_photon_shared_transport::SharedTransport`] as the
//! per-service [`Codec`]: encodes typed commands into LF-terminated ASCII frames
//! and decodes incoming CRLF-terminated frames into typed responses. Every
//! command produces exactly one response frame and the device emits no
//! unsolicited frames, so [`Codec::matches`] only enforces command→response
//! shape pairing and [`Codec::max_skip`] keeps its default of 0.
//!
//! Echo content validation (e.g. checking that `M:5000` came back as `M:5000`,
//! not `M:6000`) is the manager's job — the codec hands back the
//! [`ScopsResponse::Echo`] variant verbatim and [`crate::manager::FocuserManager`]
//! cross-checks it via [`crate::protocol::validate_echo`].

use std::str::Utf8Error;

use rusty_photon_shared_transport::{Codec, SessionError, TransportError};
use thiserror::Error;

use crate::error::ScopsOagError;
use crate::protocol::{parse_status, Command, ScopsStatus, STATUS_TOKEN};

/// Decoded response frame from the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopsResponse {
    /// `OK_SCOPS` — the `#` handshake ack.
    Handshake,
    /// `OK_SCOPS:...` — the `A` consolidated status report.
    Status(ScopsStatus),
    /// `M:<pos>` / `W:<pos>` — echo-bearing replies. The manager validates the
    /// echo's content against the issued command.
    Echo(String),
    /// A bare integer flag (`0`) — the `H` halt reply.
    Halted,
}

/// Codec-side error type.
///
/// Carries enough variants to flatten a full [`SessionError<ScopsCodecError>`]
/// in handshake / poll contexts without losing the structural information the
/// device-layer mapping (`From<SessionError<…>> for ScopsOagError`) then
/// re-expands into the right [`ScopsOagError`] variant. `Transport` carries the
/// underlying [`TransportError`] structurally so a transport-level failure
/// surfaced *through* the handshake hook stays classified.
#[derive(Debug, Error)]
pub enum ScopsCodecError {
    #[error("invalid UTF-8 in response: {0}")]
    Utf8(#[from] Utf8Error),
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("device returned an error: {0}")]
    DeviceError(String),
    #[error(transparent)]
    Transport(TransportError),
    #[error("device returned non-matching response ({0} frame(s) read)")]
    SkipExhausted(usize),
}

impl ScopsCodecError {
    /// Tighten a protocol-layer [`ScopsOagError`] into the codec-side error type.
    fn from_protocol(err: ScopsOagError) -> Self {
        match err {
            ScopsOagError::InvalidResponse(s) => Self::InvalidResponse(s),
            ScopsOagError::ParseError(s) => Self::Parse(s),
            other => Self::InvalidResponse(other.to_string()),
        }
    }
}

impl From<SessionError<Self>> for ScopsCodecError {
    fn from(err: SessionError<Self>) -> Self {
        match err {
            SessionError::Transport(t) => Self::Transport(t),
            SessionError::Codec(c) => c,
            SessionError::SkipExhausted(n) => Self::SkipExhausted(n),
        }
    }
}

/// Zero-sized codec for the Scops OAG wire protocol.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScopsCodec;

impl Codec for ScopsCodec {
    type Command = Command;
    type Response = ScopsResponse;
    type Error = ScopsCodecError;

    fn encode(&self, cmd: &Self::Command) -> Vec<u8> {
        let mut bytes = cmd.to_command_string().into_bytes();
        bytes.push(b'\n');
        bytes
    }

    fn decode(&self, bytes: &[u8]) -> Result<Self::Response, Self::Error> {
        let text = std::str::from_utf8(bytes)?.trim();

        // Order matters: the exact-match `OK_SCOPS` handshake must take
        // precedence over the `OK_SCOPS:` status prefix.
        if text == STATUS_TOKEN {
            return Ok(ScopsResponse::Handshake);
        }
        if let Some(rest) = text.strip_prefix(STATUS_TOKEN) {
            if rest.starts_with(':') {
                return parse_status(text)
                    .map(ScopsResponse::Status)
                    .map_err(ScopsCodecError::from_protocol);
            }
        }
        // `ERR:` is what the firmware returns for an unsupported command (e.g.
        // `N:`/`C:`, which this driver never sends). Surface it as a device
        // error rather than misparsing it.
        if text.starts_with("ERR") {
            return Err(ScopsCodecError::DeviceError(text.to_string()));
        }
        // Echo-bearing replies (`M:`, `W:`). The manager validates content.
        if text.starts_with("M:") || text.starts_with("W:") {
            return Ok(ScopsResponse::Echo(text.to_string()));
        }
        // A bare integer flag is the `H` halt reply.
        if text.parse::<i64>().is_ok() {
            return Ok(ScopsResponse::Halted);
        }
        Err(ScopsCodecError::InvalidResponse(format!(
            "unrecognised frame: {text:?}"
        )))
    }

    fn matches(&self, cmd: &Self::Command, resp: &Self::Response) -> bool {
        matches!(
            (cmd, resp),
            (Command::Handshake, ScopsResponse::Handshake)
                | (Command::Status, ScopsResponse::Status(_))
                | (
                    Command::MoveAbsolute { .. } | Command::SyncPosition { .. },
                    ScopsResponse::Echo(_),
                )
                | (Command::Halt, ScopsResponse::Halted)
        )
    }
}

/// Bridge [`SessionError<ScopsCodecError>`] into [`ScopsOagError`]. The mapping
/// pins which ASCOM code each failure surfaces as via
/// [`ScopsOagError::to_ascom_error`].
impl From<SessionError<ScopsCodecError>> for ScopsOagError {
    fn from(err: SessionError<ScopsCodecError>) -> Self {
        match err {
            // Both alternatives route through `From<TransportError> for
            // ScopsOagError` so a timeout that surfaces *through* the
            // handshake hook (codec alternative) gets the same
            // classification as a steady-state timeout.
            SessionError::Transport(t) | SessionError::Codec(ScopsCodecError::Transport(t)) => {
                t.into()
            }
            SessionError::Codec(ScopsCodecError::InvalidResponse(s)) => Self::InvalidResponse(s),
            SessionError::Codec(ScopsCodecError::Parse(s)) => Self::ParseError(s),
            SessionError::Codec(ScopsCodecError::DeviceError(s)) => {
                Self::Communication(format!("device returned an error: {s}"))
            }
            SessionError::Codec(c @ ScopsCodecError::Utf8(_)) => {
                Self::InvalidResponse(c.to_string())
            }
            SessionError::Codec(ScopsCodecError::SkipExhausted(n))
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
    use proptest::prelude::*;

    // ---- encode -----------------------------------------------------------

    #[test]
    fn encode_appends_newline_terminator() {
        assert_eq!(&ScopsCodec.encode(&Command::Handshake), b"#\n");
        assert_eq!(&ScopsCodec.encode(&Command::Status), b"A\n");
        assert_eq!(&ScopsCodec.encode(&Command::Halt), b"H\n");
    }

    #[test]
    fn encode_move_absolute_uses_clean_pegasus_form() {
        let bytes = ScopsCodec.encode(&Command::MoveAbsolute { position: 22000 });
        assert_eq!(&bytes, b"M:22000\n");
    }

    // ---- decode -----------------------------------------------------------

    #[test]
    fn decode_handshake_ack() {
        assert_eq!(
            ScopsCodec.decode(b"OK_SCOPS\r\n").unwrap(),
            ScopsResponse::Handshake
        );
    }

    #[test]
    fn decode_status_report() {
        let resp = ScopsCodec
            .decode(b"OK_SCOPS:1.2:1:0:22000:0:1:0:1:0\r\n")
            .unwrap();
        match resp {
            ScopsResponse::Status(s) => {
                assert_eq!(s.position, 22000);
                assert!(!s.is_moving);
                assert_eq!(s.firmware_version, "1.2");
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn decode_move_echo_passed_through() {
        assert_eq!(
            ScopsCodec.decode(b"M:5000\r\n").unwrap(),
            ScopsResponse::Echo("M:5000".to_string())
        );
    }

    #[test]
    fn decode_sync_echo_passed_through() {
        assert_eq!(
            ScopsCodec.decode(b"W:22000\r\n").unwrap(),
            ScopsResponse::Echo("W:22000".to_string())
        );
    }

    #[test]
    fn decode_halt_flag() {
        assert_eq!(ScopsCodec.decode(b"0\r\n").unwrap(), ScopsResponse::Halted);
    }

    #[test]
    fn decode_err_frame_is_device_error() {
        // `N:`/`C:` are rejected by firmware 1.2 with `ERR:`. The driver never
        // sends them, but a stray ERR must surface as a device error.
        let err = ScopsCodec.decode(b"ERR:\r\n").unwrap_err();
        assert!(matches!(err, ScopsCodecError::DeviceError(_)));
    }

    #[test]
    fn decode_malformed_status_is_parse_or_invalid() {
        let err = ScopsCodec
            .decode(b"OK_SCOPS:1.2:1:0:nope:0:1:0:1:0\r\n")
            .unwrap_err();
        assert!(matches!(
            err,
            ScopsCodecError::Parse(_) | ScopsCodecError::InvalidResponse(_)
        ));
    }

    #[test]
    fn decode_unrecognised_frame_errors() {
        let err = ScopsCodec.decode(b"WHAT\r\n").unwrap_err();
        assert!(matches!(err, ScopsCodecError::InvalidResponse(_)));
    }

    #[test]
    fn decode_invalid_utf8_errors() {
        let err = ScopsCodec.decode(&[0xFF, 0xFE]).unwrap_err();
        assert!(matches!(err, ScopsCodecError::Utf8(_)));
    }

    // ---- matches ----------------------------------------------------------

    #[test]
    fn matches_pairs_commands_with_response_shapes() {
        let c = ScopsCodec;
        assert!(c.matches(&Command::Handshake, &ScopsResponse::Handshake));
        assert!(c.matches(
            &Command::Status,
            &ScopsResponse::Status(ScopsStatus {
                firmware_version: "1.2".into(),
                position: 0,
                is_moving: false,
            })
        ));
        assert!(c.matches(
            &Command::MoveAbsolute { position: 1 },
            &ScopsResponse::Echo("M:1".into())
        ));
        assert!(c.matches(
            &Command::SyncPosition { position: 1 },
            &ScopsResponse::Echo("W:1".into())
        ));
        assert!(c.matches(&Command::Halt, &ScopsResponse::Halted));
    }

    #[test]
    fn matches_rejects_cross_shape_pairs() {
        let c = ScopsCodec;
        assert!(!c.matches(&Command::Handshake, &ScopsResponse::Halted));
        assert!(!c.matches(&Command::Status, &ScopsResponse::Echo("M:1".into())));
        assert!(!c.matches(
            &Command::MoveAbsolute { position: 1 },
            &ScopsResponse::Handshake
        ));
    }

    // ---- From<SessionError<ScopsCodecError>> for ScopsOagError ------------

    #[test]
    fn session_transport_timeout_maps_to_timeout() {
        let err: SessionError<ScopsCodecError> =
            SessionError::Transport(TransportError::Timeout(std::time::Duration::from_secs(2)));
        assert!(matches!(
            ScopsOagError::from(err),
            ScopsOagError::Timeout(_)
        ));
    }

    #[test]
    fn session_codec_device_error_maps_to_communication() {
        let err: SessionError<ScopsCodecError> =
            SessionError::Codec(ScopsCodecError::DeviceError("ERR:".into()));
        assert!(matches!(
            ScopsOagError::from(err),
            ScopsOagError::Communication(_)
        ));
    }

    #[test]
    fn session_codec_parse_maps_to_parse_error() {
        let err: SessionError<ScopsCodecError> =
            SessionError::Codec(ScopsCodecError::Parse("p".into()));
        assert!(matches!(ScopsOagError::from(err), ScopsOagError::ParseError(s) if s == "p"));
    }

    #[test]
    fn session_codec_transport_eof_maps_to_communication() {
        let err: SessionError<ScopsCodecError> =
            SessionError::Codec(ScopsCodecError::Transport(TransportError::Eof));
        assert!(matches!(
            ScopsOagError::from(err),
            ScopsOagError::Communication(_)
        ));
    }

    #[test]
    fn session_to_codec_error_flatten_preserves_transport() {
        let err: ScopsCodecError =
            SessionError::<ScopsCodecError>::Transport(TransportError::Eof).into();
        assert!(matches!(
            err,
            ScopsCodecError::Transport(TransportError::Eof)
        ));
    }

    // ---- property: any valid status frame round-trips through decode -------

    proptest! {
        #[test]
        fn decode_status_round_trips(position in -1_000_000_000i64..1_000_000_000, moving in 0u8..=1) {
            let frame = format!("OK_SCOPS:1.2:1:0:{position}:{moving}:1:0:1:0\r\n");
            let resp = ScopsCodec.decode(frame.as_bytes()).unwrap();
            match resp {
                ScopsResponse::Status(s) => {
                    prop_assert_eq!(s.position, position);
                    prop_assert_eq!(s.is_moving, moving == 1);
                }
                other => prop_assert!(false, "expected Status, got {:?}", other),
            }
        }
    }
}

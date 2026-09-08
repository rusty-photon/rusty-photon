//! Frame codec for the UPBv2 serial protocol.
//!
//! The [`Upbv2Codec`] is a zero-sized adapter that plugs into
//! [`rusty_photon_shared_transport::SharedTransport`]. It owns the bytes↔typed
//! translation for both encode and decode, plus a [`matches`](Codec::matches)
//! predicate that verifies a decoded frame is the response to the request that
//! produced it.
//!
//! Wire shape (UPBv2, firmware >= 2.4):
//!
//! * Commands are short ASCII strings terminated by `\n`.
//! * Replies are one line per request, also `\n`-terminated.
//! * Five reply shapes: the ping reply (`UPB2_OK`), the status frame
//!   (`UPB2:...` or `UPB:...`), the boot-state frame (`PS:...`), the power
//!   counters (a bare `a:b:c:d` tuple), or an echo of the command string for
//!   set commands and the firmware version.

use std::str::Utf8Error;

use rusty_photon_shared_transport::{Codec, SessionError, TransportError};
use thiserror::Error;

use crate::error::Upbv2Error;
use crate::protocol::{
    Upbv2BootState, Upbv2Command, Upbv2PowerConsumption, Upbv2Status, STATUS_PREFIXES,
};

/// Number of fields in a bare `PC` power-counter tuple.
const PC_TUPLE_LEN: usize = 4;

/// Decoded response frame from the device.
///
/// `Echo` carries the raw trimmed reply for set commands and firmware version
/// reads — the codec's [`matches`](Codec::matches) predicate validates that the
/// echo actually corresponds to the command sent.
#[derive(Debug, Clone)]
pub enum Upbv2Response {
    /// A ping reply, carried raw so the handshake can tell a UPBv2 from the
    /// PPBA that answers `PPBA_OK` on the same framing and the same USB id.
    PingReply(String),
    Status(Upbv2Status),
    PowerConsumption(Upbv2PowerConsumption),
    BootState(Upbv2BootState),
    Echo(String),
}

/// Codec-side error type.
///
/// Carries enough variants to flatten a full [`SessionError<Upbv2CodecError>`]
/// in handshake / poll-loop contexts so `?` works without losing information
/// that the device-layer `From<SessionError<…>> for Upbv2Error` then re-expands
/// into the right `Upbv2Error` variant.
///
/// `Transport` carries the underlying [`TransportError`] structurally rather
/// than as a string so a transport-level failure surfaced *through* the
/// handshake hook can still be classified as `Open` / `Io` / `Timeout` / `Eof`
/// / `Framing` by the device layer instead of collapsing to a generic
/// `Communication` error.
#[derive(Debug, Error)]
pub enum Upbv2CodecError {
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
    #[error("{0}")]
    WrongModel(String),
}

impl Upbv2CodecError {
    fn from_protocol(err: Upbv2Error) -> Self {
        match err {
            Upbv2Error::InvalidResponse(s) => Self::InvalidResponse(s),
            Upbv2Error::ParseError(s) => Self::Parse(s),
            other @ Upbv2Error::WrongModel { .. } => Self::WrongModel(other.to_string()),
            other => Self::InvalidResponse(other.to_string()),
        }
    }
}

impl From<SessionError<Self>> for Upbv2CodecError {
    fn from(err: SessionError<Self>) -> Self {
        match err {
            SessionError::Transport(t) => Self::Transport(t),
            SessionError::Codec(c) => c,
            SessionError::SkipExhausted(n) => Self::SkipExhausted(n),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Upbv2Codec;

/// Whether a frame is a bare `PC` reply.
///
/// The vendor table gives the power counters no command echo, so they arrive as
/// a naked `avgAmps:ampHours:wattHours:uptime` tuple. Nothing else the device
/// sends is four colon-separated numbers — set echoes are two tokens, `PS:` and
/// the status frame carry prefixes, and the firmware version is one — so the
/// shape identifies the frame unambiguously.
fn looks_like_power_counters(text: &str) -> bool {
    let parts: Vec<&str> = text.split(':').collect();
    parts.len() == PC_TUPLE_LEN && parts.iter().all(|p| p.parse::<f64>().is_ok())
}

impl Codec for Upbv2Codec {
    type Command = Upbv2Command;
    type Response = Upbv2Response;
    type Error = Upbv2CodecError;

    fn encode(&self, cmd: &Self::Command) -> Vec<u8> {
        let mut bytes = cmd.to_command_string().into_bytes();
        bytes.push(b'\n');
        bytes
    }

    fn decode(&self, bytes: &[u8]) -> Result<Self::Response, Self::Error> {
        let text = std::str::from_utf8(bytes)?.trim();

        if text.ends_with("_OK") {
            return Ok(Upbv2Response::PingReply(text.to_string()));
        }
        if STATUS_PREFIXES.iter().any(|p| text.starts_with(p)) {
            return text
                .parse::<Upbv2Status>()
                .map(Upbv2Response::Status)
                .map_err(Upbv2CodecError::from_protocol);
        }
        if text.starts_with("PS:") {
            return text
                .parse::<Upbv2BootState>()
                .map(Upbv2Response::BootState)
                .map_err(Upbv2CodecError::from_protocol);
        }
        if text.starts_with("PC:") || looks_like_power_counters(text) {
            return text
                .parse::<Upbv2PowerConsumption>()
                .map(Upbv2Response::PowerConsumption)
                .map_err(Upbv2CodecError::from_protocol);
        }
        Ok(Upbv2Response::Echo(text.to_string()))
    }

    fn matches(&self, cmd: &Self::Command, resp: &Self::Response) -> bool {
        match (cmd, resp) {
            // A foreign `*_OK` still matches the ping: the handshake needs the
            // frame in hand to name the model in its error rather than
            // reporting a bare skip-budget exhaustion.
            (Upbv2Command::Ping, Upbv2Response::PingReply(_))
            | (Upbv2Command::Status, Upbv2Response::Status(_))
            | (Upbv2Command::PowerConsumption, Upbv2Response::PowerConsumption(_))
            | (Upbv2Command::BootState, Upbv2Response::BootState(_))
            // The firmware-version alternative accepts any echo body: the wire
            // protocol gives a bare `n.n`.
            | (Upbv2Command::FirmwareVersion, Upbv2Response::Echo(_)) => true,
            // Set commands echo their command string.
            (
                Upbv2Command::SetOutput(..)
                | Upbv2Command::SetDew(..)
                | Upbv2Command::SetVariableVoltage(_)
                | Upbv2Command::SetUsb(..),
                Upbv2Response::Echo(echo),
            ) => echo.starts_with(&cmd.to_command_string()),
            _ => false,
        }
    }
}

impl From<SessionError<Upbv2CodecError>> for Upbv2Error {
    fn from(err: SessionError<Upbv2CodecError>) -> Self {
        match err {
            // Both alternatives route through `From<TransportError> for
            // Upbv2Error` in error.rs so a timeout that surfaces *through* the
            // handshake hook (codec alternative) gets the same classification
            // as one that surfaces on a steady-state request (transport
            // alternative).
            SessionError::Transport(t) | SessionError::Codec(Upbv2CodecError::Transport(t)) => {
                t.into()
            }
            SessionError::Codec(Upbv2CodecError::InvalidResponse(s)) => Self::InvalidResponse(s),
            SessionError::Codec(Upbv2CodecError::Parse(s)) => Self::ParseError(s),
            SessionError::Codec(Upbv2CodecError::WrongModel(s)) => Self::Communication(s),
            SessionError::Codec(c @ Upbv2CodecError::Utf8(_)) => {
                Self::InvalidResponse(c.to_string())
            }
            SessionError::Codec(Upbv2CodecError::SkipExhausted(n)) => Self::Communication(format!(
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
    use crate::protocol::{DewChannel, OutputId, PwmDuty, UsbPortId, VariableVolts};
    use rusty_photon_shared_transport::TransportError;

    const SAMPLE_PA: &[u8] =
        b"UPB2:12.5:2.4:30:25.0:60:16.5:1101:111101:128:64:0:480:960:0:240:240:96:350:0000000:0\n";

    // ---- encode ----------------------------------------------------------

    /// The five frames rig2's UPBv2 actually returned, in the order the
    /// handshake asks for them, byte for byte including the CRLF the box
    /// sends (the vendor table documents LF; the firmware sends both).
    ///
    /// The handshake matches on the response *variant*, so a frame that
    /// decodes to the wrong one fails connect just as surely as a parse
    /// error. This walks the real bytes through `decode` and asserts the
    /// variant each handshake step requires.
    #[test]
    fn the_real_handshake_sequence_decodes_to_the_variants_connect_expects() {
        let ping = Upbv2Codec.decode(b"UPB2_OK\r\n").unwrap();
        assert!(
            matches!(ping, Upbv2Response::PingReply(ref r) if r == "UPB2_OK"),
            "P# must decode as a ping reply, got {ping:?}"
        );

        let version = Upbv2Codec.decode(b"2.4\r\n").unwrap();
        assert!(
            matches!(version, Upbv2Response::Echo(ref v) if v == "2.4"),
            "PV must decode as an echo, got {version:?}"
        );

        let status = Upbv2Codec
            .decode(b"UPB2:12.7:0.0:0:40.9:32:20.9:0010:110001:0:0:0:0:0:0:0:0:0:0:0000000:1\r\n")
            .unwrap();
        assert!(
            matches!(status, Upbv2Response::Status(_)),
            "PA must decode as status, got {status:?}"
        );

        let power = Upbv2Codec
            .decode(b"0.17:14.56:184.93:305389357\r\n")
            .unwrap();
        assert!(
            matches!(power, Upbv2Response::PowerConsumption(_)),
            "PC must decode as power counters, got {power:?}"
        );

        let boot = Upbv2Codec.decode(b"PS:110:6\r\n").unwrap();
        assert!(
            matches!(boot, Upbv2Response::BootState(_)),
            "PS must decode as boot state, got {boot:?}"
        );
    }

    /// `PV` returns a bare decimal, which is one colon-separated token — so
    /// it must not be mistaken for the prefix-less `PC` tuple.
    #[test]
    fn a_bare_firmware_version_is_not_mistaken_for_power_counters() {
        let decoded = Upbv2Codec.decode(b"2.4\n").unwrap();
        assert!(matches!(decoded, Upbv2Response::Echo(_)), "{decoded:?}");
    }

    #[test]
    fn encode_appends_newline_terminator() {
        assert_eq!(&Upbv2Codec.encode(&Upbv2Command::Ping), b"P#\n");
    }

    #[test]
    fn encode_set_command_includes_argument() {
        let cmd = Upbv2Command::SetDew(DewChannel::A, PwmDuty(200));
        assert_eq!(&Upbv2Codec.encode(&cmd), b"P5:200\n");
    }

    // ---- decode ----------------------------------------------------------

    #[test]
    fn decode_ping_reply_keeps_the_raw_text() {
        let resp = Upbv2Codec.decode(b"UPB2_OK\n").unwrap();
        let Upbv2Response::PingReply(text) = resp else {
            panic!("expected PingReply, got {resp:?}");
        };
        assert_eq!(text, "UPB2_OK");
    }

    #[test]
    fn decode_keeps_a_foreign_ping_reply_rather_than_discarding_it() {
        // The handshake needs the frame to name the model in its error.
        let resp = Upbv2Codec.decode(b"PPBA_OK\n").unwrap();
        let Upbv2Response::PingReply(text) = resp else {
            panic!("expected PingReply, got {resp:?}");
        };
        assert_eq!(text, "PPBA_OK");
    }

    #[test]
    fn decode_status_strips_terminator_and_parses() {
        let resp = Upbv2Codec.decode(SAMPLE_PA).unwrap();
        let Upbv2Response::Status(status) = resp else {
            panic!("expected Status, got {resp:?}");
        };
        assert_eq!(status.voltage, 12.5);
        assert_eq!(status.dew_duty, [128, 64, 0]);
    }

    #[test]
    fn decode_status_accepts_the_upb_prefix_variant() {
        let frame = b"UPB:12.5:2.4:30:25.0:60:16.5:1101:111101:128:64:0:480:960:0:240:240:96:350:0000000:0\n";
        let resp = Upbv2Codec.decode(frame).unwrap();
        assert!(matches!(resp, Upbv2Response::Status(_)), "got {resp:?}");
    }

    #[test]
    fn decode_bare_power_counter_tuple_as_power_consumption() {
        let resp = Upbv2Codec.decode(b"1.85:0.42:5.1:3600000\n").unwrap();
        let Upbv2Response::PowerConsumption(pc) = resp else {
            panic!("expected PowerConsumption, got {resp:?}");
        };
        assert_eq!(pc.average_amps, 1.85);
    }

    #[test]
    fn decode_boot_state() {
        let resp = Upbv2Codec.decode(b"PS:1101:12\n").unwrap();
        let Upbv2Response::BootState(ps) = resp else {
            panic!("expected BootState, got {resp:?}");
        };
        assert_eq!(ps.variable_volts, 12);
    }

    #[test]
    fn decode_set_echo_as_echo() {
        let resp = Upbv2Codec.decode(b"P1:1\n").unwrap();
        let Upbv2Response::Echo(echo) = resp else {
            panic!("expected Echo, got {resp:?}");
        };
        assert_eq!(echo, "P1:1");
    }

    #[test]
    fn decode_firmware_version_as_echo() {
        let resp = Upbv2Codec.decode(b"1.5\n").unwrap();
        assert!(matches!(resp, Upbv2Response::Echo(_)), "got {resp:?}");
    }

    #[test]
    fn decode_rejects_invalid_utf8() {
        let err = Upbv2Codec.decode(&[0xff, 0xfe]).unwrap_err();
        assert!(matches!(err, Upbv2CodecError::Utf8(_)), "got {err:?}");
    }

    #[test]
    fn decode_surfaces_a_malformed_status_frame_as_a_codec_error() {
        let err = Upbv2Codec.decode(b"UPB2:12.5:2.4\n").unwrap_err();
        assert!(
            matches!(err, Upbv2CodecError::InvalidResponse(_)),
            "got {err:?}"
        );
    }

    // ---- power-counter shape discrimination ------------------------------

    #[test]
    fn a_four_number_tuple_is_recognised_as_power_counters() {
        assert!(looks_like_power_counters("1.85:0.42:5.1:3600000"));
    }

    #[test]
    fn a_set_echo_is_not_mistaken_for_power_counters() {
        // Two tokens, not four — and `P1` is not a number.
        assert!(!looks_like_power_counters("P1:1"));
        assert!(!looks_like_power_counters("P5:200"));
    }

    #[test]
    fn a_boot_state_frame_is_not_mistaken_for_power_counters() {
        assert!(!looks_like_power_counters("PS:1101:12"));
    }

    #[test]
    fn a_firmware_version_is_not_mistaken_for_power_counters() {
        assert!(!looks_like_power_counters("1.5"));
    }

    // ---- matches ---------------------------------------------------------

    #[test]
    fn ping_matches_a_ping_reply() {
        let resp = Upbv2Response::PingReply("UPB2_OK".to_string());
        assert!(Upbv2Codec.matches(&Upbv2Command::Ping, &resp));
    }

    #[test]
    fn status_matches_a_status_frame() {
        let Upbv2Response::Status(status) = Upbv2Codec.decode(SAMPLE_PA).unwrap() else {
            panic!("sample frame should decode as Status");
        };
        let resp = Upbv2Response::Status(status);
        assert!(Upbv2Codec.matches(&Upbv2Command::Status, &resp));
    }

    #[test]
    fn power_consumption_does_not_match_a_status_frame() {
        let resp = Upbv2Codec.decode(SAMPLE_PA).unwrap();
        assert!(!Upbv2Codec.matches(&Upbv2Command::PowerConsumption, &resp));
    }

    #[test]
    fn boot_state_matches_only_a_boot_state_frame() {
        let ps = Upbv2Codec.decode(b"PS:1101:12\n").unwrap();
        let pc = Upbv2Codec.decode(b"1.85:0.42:5.1:3600000\n").unwrap();
        assert!(Upbv2Codec.matches(&Upbv2Command::BootState, &ps));
        assert!(!Upbv2Codec.matches(&Upbv2Command::BootState, &pc));
    }

    #[test]
    fn firmware_version_matches_any_echo() {
        let resp = Upbv2Response::Echo("1.5".to_string());
        assert!(Upbv2Codec.matches(&Upbv2Command::FirmwareVersion, &resp));
    }

    #[test]
    fn a_set_command_matches_only_its_own_echo() {
        let cmd = Upbv2Command::SetOutput(OutputId::ALL[0], true);
        let right = Upbv2Response::Echo("P1:1".to_string());
        let wrong = Upbv2Response::Echo("P2:1".to_string());
        assert!(Upbv2Codec.matches(&cmd, &right));
        assert!(!Upbv2Codec.matches(&cmd, &wrong));
    }

    #[test]
    fn every_set_family_matches_its_echo() {
        let cases = [
            Upbv2Command::SetOutput(OutputId::ALL[2], false),
            Upbv2Command::SetDew(DewChannel::C, PwmDuty(255)),
            Upbv2Command::SetVariableVoltage(VariableVolts::new(5).unwrap()),
            Upbv2Command::SetUsb(UsbPortId::ALL[5], true),
        ];
        for cmd in cases {
            let echo = Upbv2Response::Echo(cmd.to_command_string());
            assert!(Upbv2Codec.matches(&cmd, &echo), "{cmd:?} did not match");
        }
    }

    #[test]
    fn a_status_frame_does_not_answer_a_set_command() {
        let cmd = Upbv2Command::SetOutput(OutputId::ALL[0], true);
        let resp = Upbv2Codec.decode(SAMPLE_PA).unwrap();
        assert!(!Upbv2Codec.matches(&cmd, &resp));
    }

    // ---- error conversion ------------------------------------------------

    #[test]
    fn a_transport_timeout_keeps_its_classification_through_the_codec() {
        let err = SessionError::<Upbv2CodecError>::Transport(TransportError::Timeout(
            std::time::Duration::from_secs(3),
        ));
        let converted: Upbv2Error = err.into();
        assert!(
            matches!(converted, Upbv2Error::Timeout(_)),
            "got {converted:?}"
        );
    }

    #[test]
    fn a_transport_error_wrapped_by_the_codec_classifies_the_same_way() {
        let err = SessionError::Codec(Upbv2CodecError::Transport(TransportError::Timeout(
            std::time::Duration::from_secs(3),
        )));
        let converted: Upbv2Error = err.into();
        assert!(
            matches!(converted, Upbv2Error::Timeout(_)),
            "got {converted:?}"
        );
    }

    #[test]
    fn a_codec_parse_error_becomes_a_driver_parse_error() {
        let err = SessionError::Codec(Upbv2CodecError::Parse("bad temperature".to_string()));
        let converted: Upbv2Error = err.into();
        assert!(
            matches!(converted, Upbv2Error::ParseError(_)),
            "got {converted:?}"
        );
    }

    #[test]
    fn an_exhausted_skip_budget_is_reported_with_its_frame_count() {
        let converted: Upbv2Error = SessionError::<Upbv2CodecError>::SkipExhausted(3).into();
        let Upbv2Error::Communication(msg) = converted else {
            panic!("expected Communication, got {converted:?}");
        };
        assert!(msg.contains('3'), "message should name the count: {msg}");
    }
}

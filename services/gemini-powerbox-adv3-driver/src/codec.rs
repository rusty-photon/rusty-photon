//! Frame codec for the Gemini `>X#` / `*X…#` protocol.
//!
//! [`GeminiCodec`] is the zero-sized adapter that plugs into
//! [`rusty_photon_shared_transport::SharedTransport`]. It owns the
//! bytes↔typed translation, the [`matches`](Codec::matches) predicate that
//! keeps a streamed frame from answering the wrong request, and the two
//! knobs this device needs from the shared crate: a skip budget of 2, and a
//! discard of the input buffer before every command the hub's unsolicited
//! `*G` stream could otherwise answer.
//!
//! Wire shape:
//!
//! * A command is `>X…#` and a newline. A set is sent together with `>G#`,
//!   in one buffer, so the frame that follows confirms it.
//! * A reply is `*X…#` with no line ending, `X` echoing the command letter.
//!   The frame terminator is `#`.
//! * A reply starts at the **last** `*` in the chunk. The hub cuts a
//!   periodic frame short when a command arrives and sends the reply right
//!   behind it, so what the terminator delivers can be
//!   `*G1111D…19.86D12*HGeminiPowerBoxPlusAdv3#` in one piece — measured on
//!   the dev box's unit at the first connect after a session that had left
//!   it streaming. No reply carries a `*` of its own, so the last one is
//!   where the answer begins and everything before it is the cut frame.
//! * From there, frames are sorted by their letter: `H`, `V` and `G` are
//!   parsed; a chunk with no `*` at all — the tail of a frame a discard
//!   cut — or a `*` with another letter is [`GeminiResponse::Unrelated`],
//!   which no request accepts and the skip budget absorbs.

use std::str::Utf8Error;

use rusty_photon_shared_transport::{Codec, SessionError, TransportError};
use thiserror::Error;

use crate::error::GeminiError;
use crate::protocol::{parse_firmware, GeminiCommand, StatusFrame};

/// Non-matching frames a request tolerates before it fails: the tail of a
/// frame mid-flight when the input was discarded, plus one whole periodic
/// frame whose first byte landed after the discard.
pub const MAX_SKIP: usize = 2;

/// A decoded reply.
#[derive(Debug, Clone, PartialEq)]
pub enum GeminiResponse {
    /// `*H…#`: the identity text, carried raw so the handshake can name
    /// whatever answered.
    Identity(String),
    /// `*V…#`: the firmware as its three-digit integer.
    Firmware(u16),
    /// `*G…#`: the status frame.
    Status(StatusFrame),
    /// A frame that is none of the three: skipped, never an error.
    Unrelated,
}

/// Codec-side error type.
///
/// Carries enough variants to flatten a full [`SessionError<GeminiCodecError>`]
/// in the handshake and poll-loop contexts so `?` works without losing
/// information that the device-layer `From<SessionError<…>> for GeminiError`
/// then re-expands into the right [`GeminiError`] variant. `Handshake` is
/// the connect refusals — wrong model, firmware too old, no identity reply —
/// each with its complete message, so the words the operator needs travel
/// out of the handshake hook intact.
#[derive(Debug, Error)]
pub enum GeminiCodecError {
    #[error("invalid UTF-8 in response: {0}")]
    Utf8(#[from] Utf8Error),
    #[error("parse error: {0}")]
    Parse(String),
    #[error(transparent)]
    Transport(TransportError),
    #[error("device returned non-matching response ({0} frame(s) read)")]
    SkipExhausted(usize),
    #[error("{0}")]
    Handshake(GeminiError),
}

impl From<SessionError<Self>> for GeminiCodecError {
    fn from(err: SessionError<Self>) -> Self {
        match err {
            SessionError::Transport(t) => Self::Transport(t),
            SessionError::Codec(c) => c,
            SessionError::SkipExhausted(n) => Self::SkipExhausted(n),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GeminiCodec;

/// The reply in a received chunk: its letter and the text between that and
/// the terminator. The reply begins at the chunk's last `*` — anything
/// before it is a periodic frame the hub cut short to answer. `None` when
/// the chunk holds no `*` at all.
fn payload(text: &str) -> Option<(u8, &str)> {
    let (_, rest) = text.rsplit_once('*')?;
    let letter = rest.bytes().next()?;
    let body = rest.get(1..)?;
    Some((letter, body.strip_suffix('#').unwrap_or(body)))
}

impl Codec for GeminiCodec {
    type Command = GeminiCommand;
    type Response = GeminiResponse;
    type Error = GeminiCodecError;

    fn encode(&self, cmd: &Self::Command) -> Vec<u8> {
        let mut bytes = cmd.wire().into_bytes();
        bytes.push(b'\n');
        bytes
    }

    fn decode(&self, bytes: &[u8]) -> Result<Self::Response, Self::Error> {
        let text = std::str::from_utf8(bytes)?;
        let Some((letter, body)) = payload(text) else {
            return Ok(GeminiResponse::Unrelated);
        };
        match letter {
            b'H' => Ok(GeminiResponse::Identity(body.to_string())),
            b'V' => parse_firmware(body)
                .map(GeminiResponse::Firmware)
                .map_err(|e| GeminiCodecError::Parse(e.to_string())),
            b'G' => body
                .parse::<StatusFrame>()
                .map(GeminiResponse::Status)
                .map_err(|e| GeminiCodecError::Parse(e.to_string())),
            _ => Ok(GeminiResponse::Unrelated),
        }
    }

    fn matches(&self, cmd: &Self::Command, resp: &Self::Response) -> bool {
        matches!(
            (cmd, resp),
            (GeminiCommand::Identity, GeminiResponse::Identity(_))
                | (GeminiCommand::Firmware, GeminiResponse::Firmware(_))
                | (
                    GeminiCommand::Status | GeminiCommand::Set(_),
                    GeminiResponse::Status(_)
                )
        )
    }

    fn max_skip(&self) -> usize {
        MAX_SKIP
    }

    /// Everything that expects a `*G` frame back: the poll, and every set,
    /// whose readback must be the frame the hub produced *after* acting on
    /// it. `>H#` and `>V#` are not: their replies cannot be mistaken for a
    /// streamed frame, and the skip budget covers one that arrives first.
    fn discard_before_send(&self, cmd: &Self::Command) -> bool {
        matches!(cmd, GeminiCommand::Status | GeminiCommand::Set(_))
    }
}

impl From<SessionError<GeminiCodecError>> for GeminiError {
    fn from(err: SessionError<GeminiCodecError>) -> Self {
        match err {
            // Both alternatives route through `From<TransportError> for
            // GeminiError` so a timeout that surfaces *through* the
            // handshake hook (codec alternative) gets the same classification
            // as one that surfaces on a steady-state request.
            SessionError::Transport(t) | SessionError::Codec(GeminiCodecError::Transport(t)) => {
                t.into()
            }
            SessionError::Codec(GeminiCodecError::Parse(s)) => Self::ParseError(s),
            SessionError::Codec(GeminiCodecError::Handshake(e)) => e,
            SessionError::Codec(c @ GeminiCodecError::Utf8(_)) => {
                Self::InvalidResponse(c.to_string())
            }
            SessionError::Codec(GeminiCodecError::SkipExhausted(n))
            | SessionError::SkipExhausted(n) => Self::Communication(format!(
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
    use crate::protocol::{DcOutput, DewChannel, DewMode, SetCommand};

    /// The frame captured from the dev box's unit, as the transport hands it
    /// to the codec: terminator included, no line ending.
    const CAPTURED: &[u8] =
        b"*G1111D111111U1A1T1A1M1B1M100C100C 23.25S 24.47T 67.76H 18.12D12.2V 0.02C  0.36P#";

    // ---- encode ----------------------------------------------------------

    #[test]
    fn queries_encode_with_a_trailing_newline() {
        assert_eq!(&GeminiCodec.encode(&GeminiCommand::Identity), b">H#\n");
        assert_eq!(&GeminiCodec.encode(&GeminiCommand::Status), b">G#\n");
    }

    #[test]
    fn a_set_encodes_as_the_set_and_a_status_request_in_one_buffer() {
        let cmd = GeminiCommand::Set(SetCommand::Dc(DcOutput::Dc3, false));
        assert_eq!(&GeminiCodec.encode(&cmd), b">C3#\n>G#\n");
    }

    // ---- decode ----------------------------------------------------------

    #[test]
    fn the_three_replies_the_dev_box_gave_decode_to_their_variants() {
        let identity = GeminiCodec.decode(b"*HGeminiPowerBoxPlusAdv3#").unwrap();
        assert_eq!(
            identity,
            GeminiResponse::Identity("GeminiPowerBoxPlusAdv3".to_string())
        );
        let firmware = GeminiCodec.decode(b"*V308#").unwrap();
        assert_eq!(firmware, GeminiResponse::Firmware(308));
        let status = GeminiCodec.decode(CAPTURED).unwrap();
        assert!(matches!(status, GeminiResponse::Status(_)), "{status:?}");
    }

    /// What the dev box's unit answered the first `>H#` of a connect with,
    /// while still streaming from an earlier session: 63 bytes of a periodic
    /// frame it cut short, then the identity, in one 89-byte chunk.
    const CUT_FRAME_THEN_IDENTITY: &[u8] = b"*G1111D111111U1A1T1A1M1B1M100C100C 24.13S 25.66T 70.37H 19.86D12*HGeminiPowerBoxPlusAdv3#";

    #[test]
    fn a_reply_glued_to_a_frame_the_hub_cut_short_is_the_reply() {
        let resp = GeminiCodec.decode(CUT_FRAME_THEN_IDENTITY).unwrap();
        assert_eq!(
            resp,
            GeminiResponse::Identity("GeminiPowerBoxPlusAdv3".to_string())
        );
        assert!(GeminiCodec.matches(&GeminiCommand::Identity, &resp));

        // The same shape on a poll: a cut frame, then the whole answer.
        let mut glued = b"*G1111D111111U1A1T1A1M1B1M100C100C 24.13S".to_vec();
        glued.extend_from_slice(CAPTURED);
        let GeminiResponse::Status(frame) = GeminiCodec.decode(&glued).unwrap() else {
            panic!("a cut frame before a whole one must yield the whole one");
        };
        assert_eq!(frame.ambient_temperature, 24.47);
    }

    #[test]
    fn a_frame_without_the_star_prefix_is_unrelated_not_an_error() {
        // The tail of a frame that was mid-flight when the input was
        // discarded.
        let resp = GeminiCodec
            .decode(b"111U1A1T1A1M1B1M100C100C 23.25S 24.47T 67.76H 18.12D12.2V 0.02C  0.36P#")
            .unwrap();
        assert_eq!(resp, GeminiResponse::Unrelated);
    }

    #[test]
    fn a_star_frame_with_another_letter_is_unrelated() {
        assert_eq!(
            GeminiCodec.decode(b"*Xsomething#").unwrap(),
            GeminiResponse::Unrelated
        );
        assert_eq!(
            GeminiCodec.decode(b"*#").unwrap(),
            GeminiResponse::Unrelated
        );
        assert_eq!(GeminiCodec.decode(b"#").unwrap(), GeminiResponse::Unrelated);
    }

    #[test]
    fn a_malformed_status_frame_is_a_parse_error_naming_the_field() {
        let err = GeminiCodec
            .decode(b"*G1111D111111U1A1T1A1M1B1M100C100C 23.25S#")
            .unwrap_err();
        let GeminiCodecError::Parse(msg) = err else {
            panic!("expected Parse, got {err:?}");
        };
        assert!(msg.contains("ambient temperature"), "{msg}");
    }

    #[test]
    fn a_non_numeric_firmware_is_a_parse_error() {
        let err = GeminiCodec.decode(b"*Vthree#").unwrap_err();
        assert!(matches!(err, GeminiCodecError::Parse(_)), "{err:?}");
    }

    #[test]
    fn decode_rejects_invalid_utf8() {
        let err = GeminiCodec.decode(&[0x2a, 0x47, 0xff, 0xfe]).unwrap_err();
        assert!(matches!(err, GeminiCodecError::Utf8(_)), "{err:?}");
    }

    // ---- matches ---------------------------------------------------------

    #[test]
    fn each_query_matches_its_own_reply_and_no_other() {
        let identity = GeminiResponse::Identity("x".to_string());
        let firmware = GeminiResponse::Firmware(308);
        let status = GeminiCodec.decode(CAPTURED).unwrap();
        assert!(GeminiCodec.matches(&GeminiCommand::Identity, &identity));
        assert!(!GeminiCodec.matches(&GeminiCommand::Identity, &status));
        assert!(GeminiCodec.matches(&GeminiCommand::Firmware, &firmware));
        assert!(!GeminiCodec.matches(&GeminiCommand::Firmware, &identity));
        assert!(GeminiCodec.matches(&GeminiCommand::Status, &status));
        assert!(!GeminiCodec.matches(&GeminiCommand::Status, &firmware));
    }

    #[test]
    fn a_set_is_answered_by_a_status_frame() {
        let status = GeminiCodec.decode(CAPTURED).unwrap();
        let set = GeminiCommand::Set(SetCommand::DewMode(DewChannel::Dew6, DewMode::Auto));
        assert!(GeminiCodec.matches(&set, &status));
        assert!(!GeminiCodec.matches(&set, &GeminiResponse::Firmware(308)));
    }

    #[test]
    fn an_unrelated_frame_answers_nothing() {
        for cmd in [
            GeminiCommand::Identity,
            GeminiCommand::Firmware,
            GeminiCommand::Status,
            GeminiCommand::Set(SetCommand::Dc(DcOutput::Dc2, true)),
        ] {
            assert!(
                !GeminiCodec.matches(&cmd, &GeminiResponse::Unrelated),
                "{cmd:?}"
            );
        }
    }

    // ---- the two shared-crate knobs --------------------------------------

    #[test]
    fn the_skip_budget_is_two() {
        assert_eq!(GeminiCodec.max_skip(), 2);
    }

    #[test]
    fn the_input_is_discarded_before_a_poll_and_before_a_set_but_not_before_the_identity_or_firmware_query(
    ) {
        assert!(GeminiCodec.discard_before_send(&GeminiCommand::Status));
        assert!(GeminiCodec
            .discard_before_send(&GeminiCommand::Set(SetCommand::Dc(DcOutput::Dc2, true))));
        assert!(!GeminiCodec.discard_before_send(&GeminiCommand::Identity));
        assert!(!GeminiCodec.discard_before_send(&GeminiCommand::Firmware));
    }

    // ---- error conversion ------------------------------------------------

    #[test]
    fn a_transport_timeout_keeps_its_classification_through_the_codec() {
        let err = SessionError::<GeminiCodecError>::Transport(TransportError::Timeout(
            std::time::Duration::from_secs(2),
        ));
        let converted: GeminiError = err.into();
        assert!(
            matches!(converted, GeminiError::Timeout(_)),
            "{converted:?}"
        );
        let wrapped = SessionError::Codec(GeminiCodecError::Transport(TransportError::Timeout(
            std::time::Duration::from_secs(2),
        )));
        let converted: GeminiError = wrapped.into();
        assert!(
            matches!(converted, GeminiError::Timeout(_)),
            "{converted:?}"
        );
    }

    #[test]
    fn a_codec_parse_error_becomes_a_driver_parse_error() {
        let converted: GeminiError =
            SessionError::Codec(GeminiCodecError::Parse("bad humidity".to_string())).into();
        assert!(
            matches!(converted, GeminiError::ParseError(ref s) if s == "bad humidity"),
            "{converted:?}"
        );
    }

    #[test]
    fn a_handshake_refusal_comes_out_as_itself() {
        let converted: GeminiError = SessionError::Codec(GeminiCodecError::Handshake(
            GeminiError::WrongModel("device answered *HGeminiFlatPanelV3#".to_string()),
        ))
        .into();
        assert!(
            matches!(converted, GeminiError::WrongModel(_)),
            "{converted:?}"
        );
    }

    #[test]
    fn an_exhausted_skip_budget_is_reported_with_its_frame_count() {
        let converted: GeminiError = SessionError::<GeminiCodecError>::SkipExhausted(3).into();
        let GeminiError::Communication(msg) = converted else {
            panic!("expected Communication, got {converted:?}");
        };
        assert!(msg.contains("3 frames"), "{msg}");
    }
}

//! Frame codec for the Sky-Watcher motor-controller wire protocol.
//!
//! The [`SkywatcherCodec`] is the per-service plug that
//! [`rusty_photon_shared_transport::SharedTransport`] uses to translate
//! between typed [`Command`]s and on-wire frames. The wire protocol is
//! `:cmd<axis><payload?>\r` for requests and `=<body>\r` /
//! `!<code>\r` for responses; framing terminator (`\r` on serial,
//! datagram on UDP) belongs to the [`FrameTransport`] implementation.
//!
//! # Why `Response = Vec<u8>`
//!
//! The protocol crate's [`Response::decode`] needs the originating
//! [`Command`] to disambiguate replies that share a wire shape — a
//! 6-hex-byte success body decodes as [`Response::U24`] for `:a` (CPR
//! inquiry) but as [`Response::Position`] for `:j` (position inquiry).
//! The shared crate's [`Codec::decode`] signature is `(&self, &[u8]) ->
//! Result<Response, Error>` with no command in scope, so we can't do
//! the typed decode at the codec layer.
//!
//! Instead the codec validates frame structure (must start with `=` or
//! `!`, end with `\r`) and returns the raw frame bytes verbatim. The
//! [`crate::MountManager::send`] wrapper then calls the protocol
//! crate's [`Response::decode`] with the originating command, yielding
//! the typed [`Response`] all existing call sites already expect.
//!
//! [`FrameTransport`]: rusty_photon_shared_transport::FrameTransport
//! [`Response`]: skywatcher_motor_protocol::Response
//! [`Response::decode`]: skywatcher_motor_protocol::Response::decode
//! [`Response::U24`]: skywatcher_motor_protocol::Response::U24
//! [`Response::Position`]: skywatcher_motor_protocol::Response::Position
//! [`Codec::decode`]: rusty_photon_shared_transport::Codec::decode

use rusty_photon_shared_transport::{Codec, SessionError, TransportError};
use skywatcher_motor_protocol::codec::validate_response_frame;
use skywatcher_motor_protocol::{Command, ProtocolError, Response};
use thiserror::Error;

use crate::error::StarAdvError;

/// Codec-side error type. Mirrors the variants the codec layer can
/// surface (frame-structure violations) and the protocol-crate errors
/// the typed-decode step produces in [`crate::MountManager::send`].
///
/// `Transport` carries the underlying [`TransportError`] structurally
/// rather than as a string so a transport-level failure surfaced
/// *through* the handshake hook (which returns
/// `Result<_, SkywatcherCodecError>`) can still be classified as
/// `Open` / `Io` / `Timeout` / `Eof` / `Framing` by the device layer
/// instead of collapsing to a generic `Protocol(FrameError(...))` that
/// would map to `INVALID_OPERATION` and lose the connect-time
/// `Timeout` / `ConnectionFailed` classification ASCOM clients
/// deserve. Mirrors the same fix `PpbaCodecError` and `QhyCodecError`
/// carry — see PR #280.
#[derive(Debug, Error)]
pub enum SkywatcherCodecError {
    /// The frame failed the wire-format check in
    /// [`validate_response_frame`] — wrong prefix byte, missing `\r`
    /// terminator, non-ASCII hex in the body, etc.
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    /// Wire-level I/O failure surfaced through a context that returns
    /// `Result<_, SkywatcherCodecError>` (the handshake / teardown
    /// hooks). Preserves the structured [`TransportError`] so the
    /// device-layer mapping can route the classification through one
    /// helper — see [`StarAdvError::from`] for [`SessionError`].
    #[error(transparent)]
    Transport(TransportError),
    /// The connection layer's skip budget tripped without seeing a
    /// matching frame. Carried so the surfaced [`StarAdvError`] can
    /// quote the count for log triage.
    #[error("device returned non-matching response ({0} frame(s) read)")]
    SkipExhausted(usize),
    /// The connect handshake's `:e1` identity probe came back with a
    /// reply that isn't a Sky-Watcher motor-board-version. Surfaces in
    /// any of the following situations (all routed through
    /// `wrong_device_for_e1` in `manager.rs`):
    ///
    /// - [`ProtocolError::FrameError`] — missing `=` prefix, missing
    ///   `\r` terminator, embedded `\r`, structurally invalid.
    /// - [`ProtocolError::PayloadError`] — `=...\r` arrived but with
    ///   the wrong body length / shape for the U24 motor-board-version
    ///   payload (e.g. a 3-hex-byte status-shaped body).
    /// - [`ProtocolError::HexError`] — payload bytes that aren't ASCII
    ///   hex digits.
    /// - [`ProtocolError::MountError`] — device replied with a
    ///   structurally-valid `!X\r` error frame. A real Sky-Watcher
    ///   controller supports `:e` per the protocol spec, so a mount-side
    ///   error to `:e1` indicates an incompatible device.
    /// - Mount-type byte outside the
    ///   [`skywatcher_motor_protocol::MountType`] whitelist (post-decode
    ///   check; reply was structurally valid but the type byte — the low
    ///   byte of the U24 — isn't a known Sky-Watcher mount family).
    ///
    /// Carried through this error type so the handshake hook can stop
    /// the connect sequence before issuing any device-specific command
    /// (`:F`, `:a`, `:b`, `:g`, …) and the device-layer mapping can
    /// route the structured context (port label + reason) into
    /// [`StarAdvError::WrongDevice`] for an operator-friendly diagnostic.
    /// See [issue #254][issue].
    ///
    /// [`ProtocolError::FrameError`]: skywatcher_motor_protocol::ProtocolError::FrameError
    /// [`ProtocolError::PayloadError`]: skywatcher_motor_protocol::ProtocolError::PayloadError
    /// [`ProtocolError::HexError`]: skywatcher_motor_protocol::ProtocolError::HexError
    /// [`ProtocolError::MountError`]: skywatcher_motor_protocol::ProtocolError::MountError
    /// [issue]: https://github.com/rusty-photon/rusty-photon/issues/254
    #[error("wrong device on {port}: {reason}")]
    WrongDevice { port: String, reason: String },
}

/// Flatten a [`SessionError`] arising inside a handshake / teardown
/// hook (which returns `Result<_, SkywatcherCodecError>`) into the
/// codec error type so `?` works without losing the structured
/// transport-error variant. The device-layer
/// `From<SessionError<SkywatcherCodecError>> for StarAdvError` then
/// re-expands the [`Transport`] arm via the canonical
/// `From<TransportError> for StarAdvError` impl used for top-level
/// [`SessionError::Transport`].
impl From<SessionError<Self>> for SkywatcherCodecError {
    fn from(err: SessionError<Self>) -> Self {
        match err {
            SessionError::Transport(t) => Self::Transport(t),
            SessionError::Codec(c) => c,
            SessionError::SkipExhausted(n) => Self::SkipExhausted(n),
        }
    }
}

impl SkywatcherCodecError {
    /// Build a [`SkywatcherCodecError::WrongDevice`] from a port label and
    /// a free-form reason. Lives on the codec error so the handshake hook
    /// can construct it without reaching into either the `StarAdvError`
    /// type (one layer above the hook's return-type constraint) or the
    /// protocol crate (one layer below it).
    pub fn wrong_device(port: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::WrongDevice {
            port: port.into(),
            reason: reason.into(),
        }
    }
}

/// Zero-sized adapter that plugs the Sky-Watcher wire protocol into
/// [`rusty_photon_shared_transport::SharedTransport`].
///
/// Stateless — `Clone` is required by the [`Codec`] trait (the shared
/// transport stamps a fresh codec onto every new `Connection`) and is
/// trivially satisfied by `Copy`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SkywatcherCodec;

impl Codec for SkywatcherCodec {
    type Command = Command;
    type Response = Vec<u8>;
    type Error = SkywatcherCodecError;

    fn encode(&self, cmd: &Self::Command) -> Vec<u8> {
        // `Command::encode` only fails on out-of-range numeric arguments
        // (currently the `i32` ticks fed to `encode_position` inside
        // `SetPosition` / `SetGotoTarget`).
        // [`crate::MountManager::send`] validates those variants before
        // they ever reach the codec, so this branch should be
        // unreachable in well-formed call paths.
        //
        // Defensive fallback: if a future call site bypasses the
        // manager and constructs an unencodable command, log loudly and
        // return an empty frame. The transport writes 0 bytes, the
        // peer never replies, and `Session::request` surfaces a
        // `Transport(Timeout)` to the ASCOM caller. Misleading vs the
        // real cause (a programming bug), but never a service crash —
        // see the trailing log line for the bug-report hint.
        match cmd.encode() {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::error!(
                    command = ?cmd,
                    error = %e,
                    "SkywatcherCodec::encode failed unexpectedly; returning empty frame so the \
                     downstream request surfaces as Transport(Timeout) rather than panicking. \
                     This indicates a Command was constructed without MountManager::send's \
                     range validation \u{2014} file a bug."
                );
                Vec::new()
            }
        }
    }

    fn decode(&self, bytes: &[u8]) -> Result<Self::Response, Self::Error> {
        // Normalize wire quirks the legacy bespoke transports used to
        // smooth over before the shared `FrameTransport` adapters took
        // over framing:
        //
        // * **Trailing `\n` after `\r` on UDP** — some firmware
        //   revisions append `\n` to the `\r` terminator on UDP
        //   replies; the reference doc lists this as tolerated. The
        //   shared `UdpFrameTransport` returns the whole datagram
        //   verbatim, so the `\n` survives into the codec and would
        //   fail `validate_response_frame` (which requires `\r` as
        //   the last byte).
        // * **Leading junk before the first `=`/`!` on serial** — the
        //   mount occasionally emits framing junk between frames; the
        //   legacy `SerialTransport` skipped non-`=`/`!` bytes until
        //   it found a real start. The shared `SerialFrameTransport`
        //   reads until `\r` and returns the bytes verbatim, so any
        //   leading junk lands at the head of the frame and would
        //   fail `validate_response_frame`.
        //
        // Normalize once, here, so the typed-decode boundary in
        // `MountManager::send` sees a clean `=...\r` / `!...\r` frame.
        let normalized = normalize_response_frame(bytes);
        validate_response_frame(normalized)?;
        Ok(normalized.to_vec())
    }

    // `matches`: default true. The Sky-Watcher protocol is strictly
    // request/response with no unsolicited frames — every recv after a
    // send is by definition the response to that send.
    //
    // `max_skip`: default 0. Same reasoning: no unsolicited frames to
    // skip.
}

/// Strip leading non-frame-start bytes and a trailing `\n` from a raw
/// response frame so the shared `FrameTransport` adapters don't have to
/// know about protocol-specific framing quirks.
///
/// * **Leading skip:** advance past any bytes that aren't `=` or `!`.
///   The Sky-Watcher firmware occasionally emits framing junk between
///   frames (the legacy `SerialTransport`'s read loop dropped these
///   before the `=`/`!` prefix). If no `=`/`!` is found the whole
///   buffer is returned untouched so `validate_response_frame` can
///   surface a meaningful error.
/// * **Trailing `\n` strip:** if the last two bytes are `\r\n`, drop
///   the `\n`. Some firmware revisions on UDP append `\n` after the
///   `\r` terminator; the reference doc lists this as tolerated.
fn normalize_response_frame(bytes: &[u8]) -> &[u8] {
    let body = match bytes.split_last() {
        Some((&b'\n', head)) if head.ends_with(b"\r") => head,
        _ => bytes,
    };
    body.iter()
        .position(|&b| b == b'=' || b == b'!')
        .map_or(body, |start| body.get(start..).unwrap_or(body))
}

/// Decode a raw frame against the command that produced it.
///
/// Companion to [`SkywatcherCodec`] used by [`crate::MountManager::send`]
/// (and by the connect-time handshake) to turn a [`Codec::Response`]
/// (the raw frame bytes) into the protocol-crate's typed [`Response`]
/// after the request has come back over the wire.
///
/// # Errors
///
/// Returns [`Response::decode`]'s [`ProtocolError`]: a frame that fails
/// the response framing rules, a `!` error reply (as
/// [`ProtocolError::MountError`]), a non-hex payload byte, or a payload
/// whose shape does not match `cmd`. It comes back unwrapped so call
/// sites can flow it through `?` into either [`SkywatcherCodecError`]
/// (via the `#[from]` on `Protocol`) or [`StarAdvError`] (via the
/// existing `#[from] ProtocolError`) — the wire transaction has already
/// succeeded by the time this runs, so the transport-error variants of
/// [`SkywatcherCodecError`] are unreachable here.
pub fn decode_frame_for(cmd: &Command, frame: &[u8]) -> Result<Response, ProtocolError> {
    Response::decode(frame, cmd)
}

/// Per-variant classification for the codec's own error type. The
/// `From<SessionError<…>> for StarAdvError` impl below delegates the
/// `SessionError::Codec(_)` arm here so each error layer owns its own
/// variants: adding a new `SkywatcherCodecError` variant updates one
/// match arm instead of two.
impl From<SkywatcherCodecError> for StarAdvError {
    fn from(err: SkywatcherCodecError) -> Self {
        match err {
            // Transport-arm routing delegates to the canonical
            // `From<TransportError> for StarAdvError` so a timeout
            // surfaced *through* the handshake hook (Codec arm in the
            // outer `From<SessionError<…>>`) gets the same
            // classification as one that surfaces on a steady-state
            // request (top-level Transport arm). See PR #280 for the
            // bug class.
            SkywatcherCodecError::Transport(t) => t.into(),
            SkywatcherCodecError::Protocol(pe) => Self::Protocol(pe),
            SkywatcherCodecError::SkipExhausted(n) => Self::Transport(format!(
                "device returned non-matching response ({n} frame{s} read)",
                s = if n == 1 { "" } else { "s" }
            )),
            // WrongDevice carries port-label context the handshake hook
            // captured (see `MountManager::new` in manager.rs); preserve
            // it as the structured `StarAdvError::WrongDevice` so the
            // operator-facing message lands intact in ASCOM's
            // `INVALID_OPERATION` reply.
            SkywatcherCodecError::WrongDevice { port, reason } => {
                Self::WrongDevice { port, reason }
            }
        }
    }
}

impl From<SessionError<SkywatcherCodecError>> for StarAdvError {
    fn from(err: SessionError<SkywatcherCodecError>) -> Self {
        match err {
            SessionError::Transport(t) => t.into(),
            SessionError::Codec(c) => c.into(),
            SessionError::SkipExhausted(n) => Self::Transport(format!(
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
    use skywatcher_motor_protocol::Axis;

    #[test]
    fn encode_initialize_emits_colon_prefixed_carriage_return_terminated_frame() {
        let bytes = SkywatcherCodec.encode(&Command::Initialize(Axis::Ra));
        assert_eq!(&bytes, b":F1\r");
    }

    #[test]
    fn encode_inquire_position_includes_axis_byte() {
        let bytes = SkywatcherCodec.encode(&Command::InquirePosition(Axis::Dec));
        assert_eq!(&bytes, b":j2\r");
    }

    #[test]
    fn decode_passes_through_well_formed_success_frame() {
        let frame = SkywatcherCodec.decode(b"=000080\r").unwrap();
        assert_eq!(frame, b"=000080\r");
    }

    #[test]
    fn decode_passes_through_well_formed_error_frame() {
        let frame = SkywatcherCodec.decode(b"!00\r").unwrap();
        assert_eq!(frame, b"!00\r");
    }

    #[test]
    fn decode_rejects_missing_terminator() {
        let err = SkywatcherCodec.decode(b"=000080").unwrap_err();
        assert!(matches!(err, SkywatcherCodecError::Protocol(_)));
    }

    #[test]
    fn decode_rejects_bad_prefix() {
        // `?` is neither `=` nor `!` — leading-skip walks the buffer
        // and finds no frame start, so validation sees the original
        // `?` and rejects it.
        let err = SkywatcherCodec.decode(b"?000080\r").unwrap_err();
        assert!(matches!(err, SkywatcherCodecError::Protocol(_)));
    }

    #[test]
    fn decode_strips_trailing_newline_on_udp_style_frame() {
        // Some firmware revisions append `\n` to the `\r` terminator
        // on UDP replies; the codec strips it so the rest of the
        // pipeline sees a clean `=000080\r`. Without normalization,
        // `validate_response_frame` would reject this for the wrong
        // terminator byte.
        let frame = SkywatcherCodec.decode(b"=000080\r\n").unwrap();
        assert_eq!(frame, b"=000080\r");
    }

    #[test]
    fn decode_skips_leading_junk_before_frame_start() {
        // The Sky-Watcher firmware occasionally emits framing junk
        // (stray `\n`, residue from a prior frame) between responses;
        // the legacy SerialTransport's read loop dropped these before
        // the `=`/`!` prefix. The codec must do the same so the
        // typed-decode boundary in MountManager::send sees a clean
        // frame.
        let frame = SkywatcherCodec.decode(b"\n=000080\r").unwrap();
        assert_eq!(frame, b"=000080\r");
    }

    #[test]
    fn decode_skips_leading_junk_then_error_prefix() {
        // The leading-junk skip looks for the first `=` *or* `!` —
        // error frames must survive normalization just as cleanly as
        // success frames.
        let frame = SkywatcherCodec.decode(b"\x00!00\r").unwrap();
        assert_eq!(frame, b"!00\r");
    }

    #[test]
    fn decode_handles_both_leading_junk_and_trailing_newline_together() {
        let frame = SkywatcherCodec.decode(b"\n=000080\r\n").unwrap();
        assert_eq!(frame, b"=000080\r");
    }

    #[test]
    fn normalize_passes_through_clean_frame_unchanged() {
        assert_eq!(normalize_response_frame(b"=000080\r"), b"=000080\r");
        assert_eq!(normalize_response_frame(b"!00\r"), b"!00\r");
    }

    #[test]
    fn normalize_leaves_no_frame_start_alone() {
        // If no `=`/`!` is found, return the whole buffer so
        // `validate_response_frame` surfaces a meaningful error rather
        // than the helper silently returning an empty slice.
        assert_eq!(normalize_response_frame(b"garbage"), b"garbage");
    }

    #[test]
    fn decode_frame_for_inquire_position_returns_signed_position() {
        // `:j1` returns biased 6-hex u24; `decode_frame_for` debias to i32.
        let resp = decode_frame_for(&Command::InquirePosition(Axis::Ra), b"=000080\r").unwrap();
        match resp {
            Response::Position(p) => assert_eq!(p, 0),
            other => panic!("expected Position(0), got {other:?}"),
        }
    }

    #[test]
    fn decode_frame_for_inquire_cpr_returns_unsigned_u24() {
        // `:a1` returns unsigned 6-hex u24 — same wire shape as Position
        // but a different decode.
        let resp = decode_frame_for(&Command::InquireCpr(Axis::Ra), b"=005F37\r").unwrap();
        assert_eq!(resp, Response::U24(0x0037_5F00));
    }

    #[test]
    fn decode_frame_for_error_reply_maps_to_protocol_error() {
        let err = decode_frame_for(&Command::Initialize(Axis::Ra), b"!00\r").unwrap_err();
        assert!(matches!(err, ProtocolError::MountError(_)));
    }

    #[test]
    fn session_error_transport_open_maps_to_connection_failed() {
        let err: SessionError<SkywatcherCodecError> =
            SessionError::Transport(TransportError::Open(std::io::Error::other("device busy")));
        match StarAdvError::from(err) {
            StarAdvError::ConnectionFailed(s) => assert!(s.contains("device busy")),
            other => panic!("expected ConnectionFailed, got {other:?}"),
        }
    }

    #[test]
    fn session_error_transport_timeout_maps_to_timeout() {
        let err: SessionError<SkywatcherCodecError> =
            SessionError::Transport(TransportError::Timeout(std::time::Duration::from_secs(2)));
        match StarAdvError::from(err) {
            StarAdvError::Timeout(_) => {}
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn session_error_codec_protocol_passes_through() {
        let err: SessionError<SkywatcherCodecError> = SessionError::Codec(
            SkywatcherCodecError::Protocol(ProtocolError::FrameError("bad".to_string())),
        );
        assert!(matches!(StarAdvError::from(err), StarAdvError::Protocol(_)));
    }

    #[test]
    fn session_error_skip_exhausted_maps_to_transport() {
        let err: SessionError<SkywatcherCodecError> = SessionError::SkipExhausted(3);
        match StarAdvError::from(err) {
            StarAdvError::Transport(s) => {
                assert!(s.contains('3') && s.contains("non-matching"));
            }
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[test]
    fn session_error_transport_io_preserves_io_kind() {
        // The Io branch routes the inner `io::Error` through
        // `StarAdvError::Io(_)` so the kind survives the conversion
        // — important for callers that pattern-match
        // `ErrorKind::BrokenPipe` etc.
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken");
        let err: SessionError<SkywatcherCodecError> =
            SessionError::Transport(TransportError::Io(io_err));
        match StarAdvError::from(err) {
            StarAdvError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::BrokenPipe),
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn session_error_transport_framing_maps_to_communication_with_prefix() {
        // The Framing branch flattens the wire-side framing error
        // string into `StarAdvError::Communication(...)` with a
        // "framing: " prefix (the shared `From<TransportError>` mapping)
        // so logs can tell it apart from other transport faults.
        let err: SessionError<SkywatcherCodecError> =
            SessionError::Transport(TransportError::Framing("too big".to_string()));
        match StarAdvError::from(err) {
            StarAdvError::Communication(s) => {
                assert!(s.starts_with("framing:") && s.contains("too big"));
            }
            other => panic!("expected Communication, got {other:?}"),
        }
    }

    #[test]
    fn session_error_transport_eof_maps_to_connection_closed() {
        let err: SessionError<SkywatcherCodecError> = SessionError::Transport(TransportError::Eof);
        match StarAdvError::from(err) {
            StarAdvError::Communication(s) => assert!(s.contains("Connection closed")),
            other => panic!("expected Communication, got {other:?}"),
        }
    }

    // ============================================================================
    // From<SessionError<SkywatcherCodecError>> for SkywatcherCodecError: used in
    // handshake / teardown / `request_typed` contexts so `?` flattens
    // transport-side failures into the codec error type without losing the
    // structured `TransportError` variant.
    // ============================================================================

    #[test]
    fn session_to_codec_error_transport_preserves_inner_variant() {
        // The inner TransportError must survive the flatten so the
        // device-layer mapping can classify by variant rather than
        // collapse to a stringy Protocol(FrameError).
        let err: SkywatcherCodecError =
            SessionError::<SkywatcherCodecError>::Transport(TransportError::Eof).into();
        assert!(matches!(
            err,
            SkywatcherCodecError::Transport(TransportError::Eof)
        ));
    }

    #[test]
    fn session_to_codec_error_codec_is_identity() {
        let inner = SkywatcherCodecError::Protocol(ProtocolError::FrameError("p".to_string()));
        let err: SkywatcherCodecError = SessionError::Codec(inner).into();
        assert!(matches!(err, SkywatcherCodecError::Protocol(_)));
    }

    #[test]
    fn session_to_codec_error_skip_exhausted_passes_count() {
        let err: SkywatcherCodecError =
            SessionError::<SkywatcherCodecError>::SkipExhausted(3).into();
        assert!(matches!(err, SkywatcherCodecError::SkipExhausted(3)));
    }

    // ============================================================================
    // From<SessionError<SkywatcherCodecError>> for StarAdvError: the device-layer
    // mapping that decides which ASCOMErrorCode each failure ultimately surfaces
    // as. The Codec(Transport(_)) arms below cover the connect-time-failure
    // path where a TransportError comes back wrapped in the handshake hook's
    // error type — those must classify identically to the steady-state path.
    // ============================================================================

    #[test]
    fn session_error_codec_transport_timeout_routes_to_timeout() {
        // A transport timeout surfaced *through* the handshake hook
        // arrives at the device layer as
        // SessionError::Codec(SkywatcherCodecError::Transport(Timeout(...))).
        // It must map to StarAdvError::Timeout — same classification a
        // steady-state timeout (SessionError::Transport(Timeout(...)))
        // would receive — so the ASCOM client doesn't see a generic
        // INVALID_OPERATION for connect-time timeouts.
        let err: SessionError<SkywatcherCodecError> =
            SessionError::Codec(SkywatcherCodecError::Transport(TransportError::Timeout(
                std::time::Duration::from_secs(2),
            )));
        match StarAdvError::from(err) {
            StarAdvError::Timeout(s) => assert!(s.contains('2')),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn session_error_codec_transport_open_routes_to_connection_failed() {
        let err: SessionError<SkywatcherCodecError> =
            SessionError::Codec(SkywatcherCodecError::Transport(TransportError::Open(
                std::io::Error::other("device busy"),
            )));
        match StarAdvError::from(err) {
            StarAdvError::ConnectionFailed(s) => assert!(s.contains("device busy")),
            other => panic!("expected ConnectionFailed, got {other:?}"),
        }
    }

    #[test]
    fn session_error_codec_transport_eof_routes_to_connection_closed() {
        let err: SessionError<SkywatcherCodecError> =
            SessionError::Codec(SkywatcherCodecError::Transport(TransportError::Eof));
        match StarAdvError::from(err) {
            StarAdvError::Communication(s) => assert!(s.contains("Connection closed")),
            other => panic!("expected Communication, got {other:?}"),
        }
    }

    #[test]
    fn session_error_codec_skip_exhausted_maps_to_transport_with_count() {
        let err: SessionError<SkywatcherCodecError> =
            SessionError::Codec(SkywatcherCodecError::SkipExhausted(7));
        match StarAdvError::from(err) {
            StarAdvError::Transport(s) => {
                assert!(s.contains("non-matching") && s.contains('7'));
            }
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    // ============================================================================
    // From<SkywatcherCodecError> for StarAdvError — the standalone per-variant
    // classification the outer `From<SessionError<…>>` delegates to. Tests here
    // cover the impl directly so a regression in the codec→staradv arm wouldn't
    // be masked by the outer impl's `SessionError::Codec(c) => c.into()`
    // delegation.
    // ============================================================================

    #[test]
    fn codec_error_transport_timeout_routes_to_timeout() {
        let err: StarAdvError = SkywatcherCodecError::Transport(TransportError::Timeout(
            std::time::Duration::from_secs(3),
        ))
        .into();
        match err {
            StarAdvError::Timeout(s) => assert!(s.contains('3')),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn codec_error_protocol_passes_through() {
        let err: StarAdvError =
            SkywatcherCodecError::Protocol(ProtocolError::FrameError("bad".into())).into();
        assert!(matches!(err, StarAdvError::Protocol(_)));
    }

    #[test]
    fn codec_error_skip_exhausted_maps_to_transport_with_count() {
        let err: StarAdvError = SkywatcherCodecError::SkipExhausted(2).into();
        match err {
            StarAdvError::Transport(s) => {
                assert!(s.contains("non-matching") && s.contains('2'));
            }
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[test]
    fn codec_error_wrong_device_preserves_port_and_reason() {
        // The whole point of routing WrongDevice through structured
        // fields instead of stringifying is that the operator-facing
        // diagnostic survives intact — verify the per-variant impl
        // doesn't lose either field.
        let err: StarAdvError = SkywatcherCodecError::WrongDevice {
            port: "/dev/ttyACM7".into(),
            reason: "mount-type byte 0xFF unknown".into(),
        }
        .into();
        match err {
            StarAdvError::WrongDevice { port, reason } => {
                assert_eq!(port, "/dev/ttyACM7");
                assert!(reason.contains("0xFF"));
            }
            other => panic!("expected WrongDevice, got {other:?}"),
        }
    }

    // ============================================================================
    // From<TransportError> for StarAdvError — the device-layer disconnect path
    // (and `ascom_transport_err` helper) uses this to map `Session::close()`'s
    // TransportError directly without synthesizing a SessionError. Test each
    // TransportError variant routes to its expected StarAdvError arm.
    // ============================================================================

    #[test]
    fn from_transport_error_open_maps_to_connection_failed() {
        let err: StarAdvError = TransportError::Open(std::io::Error::other("busy")).into();
        assert!(matches!(err, StarAdvError::ConnectionFailed(s) if s.contains("busy")));
    }

    #[test]
    fn from_transport_error_io_preserves_io_kind() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken");
        let err: StarAdvError = TransportError::Io(io_err).into();
        match err {
            StarAdvError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::BrokenPipe),
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn from_transport_error_timeout_maps_to_timeout() {
        let err: StarAdvError = TransportError::Timeout(std::time::Duration::from_secs(2)).into();
        assert!(matches!(err, StarAdvError::Timeout(s) if s.contains('2')));
    }

    #[test]
    fn from_transport_error_eof_maps_to_communication_connection_closed() {
        let err: StarAdvError = TransportError::Eof.into();
        assert!(matches!(err, StarAdvError::Communication(s) if s.contains("Connection closed")));
    }

    #[test]
    fn from_transport_error_framing_maps_to_communication_with_prefix() {
        let err: StarAdvError = TransportError::Framing("too big".to_string()).into();
        assert!(
            matches!(err, StarAdvError::Communication(s) if s.starts_with("framing:") && s.contains("too big"))
        );
    }
}

//! The [`Codec`] trait: translation between protocol commands and on-wire frames.
//!
//! A [`Codec`] is the per-service plug that tells [`SharedTransport`] how
//! to turn a typed command into bytes and back. Codecs operate on whole
//! frames — framing (terminator-based for serial, datagram-based for
//! UDP) is the responsibility of the [`crate::FrameTransport`]
//! implementation, not the codec.
//!
//! [`SharedTransport`]: crate::SharedTransport
//!
//! # Stale-frame skipping
//!
//! Some protocols emit unsolicited frames between request/response pairs
//! — `qhy-focuser` is the motivating example: while a focuser move is in
//! progress, the controller pushes position updates that arrive in
//! between foreground commands. A naive read-one-frame-after-write would
//! either misattribute the position update as the response or hang
//! waiting for a frame that already came and went.
//!
//! [`Codec::matches`] is the predicate that lets the connection layer
//! recognise the right response, and [`Codec::max_skip`] bounds how many
//! unsolicited frames the connection will tolerate before giving up.
//! Both have defaults that match the common "every request gets exactly
//! one matching response" case.

/// Translation between typed commands/responses and on-wire frames.
///
/// `Clone` is required because [`crate::SharedTransport`] stamps a fresh
/// codec copy onto every new [`crate::Connection`] (one per 0→1
/// connect transition). Codecs are typically zero-sized types or hold
/// only small `Copy`-able config, so the bound is cheap to satisfy.
pub trait Codec: Send + Sync + Clone + 'static {
    /// The typed command the service-level API speaks in
    /// (e.g. `QhyCommand::GetPosition`, `PpbaCommand::SetPower { .. }`).
    type Command: Send + Sync;

    /// The typed response the service-level API receives.
    type Response: Send;

    /// Codec-level error type for parse / deserialise / validate failures.
    ///
    /// Must be `Error + Send + Sync + 'static` so it can ride inside
    /// [`crate::SessionError::Codec`] and cross task boundaries.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Encode `cmd` into one whole frame's worth of bytes.
    ///
    /// The returned slice is written **verbatim** by the
    /// [`crate::FrameTransport`] — it does not insert, strip, or rewrite
    /// any bytes. The codec is therefore responsible for emitting every
    /// byte the protocol carries on the wire, including a framing
    /// terminator if the chosen transport expects one:
    ///
    /// * For [`crate::SerialFrameTransport`] (terminator-delimited),
    ///   `encode` **must** emit the configured terminator byte
    ///   (e.g. `\r`, `\n`, `}`) at the end of every frame; otherwise
    ///   the peer never sees a complete frame.
    /// * For [`crate::UdpFrameTransport`] (datagram-bounded), no
    ///   terminator is needed — each `send_frame` is one datagram.
    fn encode(&self, cmd: &Self::Command) -> Vec<u8>;

    /// Decode one whole response frame's bytes into a typed response.
    ///
    /// `bytes` is exactly what [`crate::FrameTransport::recv_frame`]
    /// returned — including any in-frame terminator the protocol carries.
    ///
    /// # Errors
    ///
    /// Returns the codec's error type when the frame is malformed or
    /// cannot be translated into a typed response.
    fn decode(&self, bytes: &[u8]) -> Result<Self::Response, Self::Error>;

    /// Return `true` iff `resp` is the legitimate response to `cmd`.
    ///
    /// Default: always-true (matches the immediately preceding request).
    /// Override only when the protocol can interleave unsolicited frames
    /// with foreground responses and the request carries a discriminator
    /// the response echoes back. qhy-focuser overrides this to compare
    /// `cmd_id ↔ idx`.
    fn matches(&self, cmd: &Self::Command, resp: &Self::Response) -> bool {
        let _ = (cmd, resp);
        true
    }

    /// Maximum number of non-matching frames the connection will skip
    /// before erroring out of [`crate::Session::request`].
    ///
    /// Default: 0 (any non-matching frame is an immediate error). qhy
    /// overrides to 5 to absorb position updates pushed during a move.
    fn max_skip(&self) -> usize {
        0
    }
}

//! Error model.
//!
//! Two enums live here. [`TransportError`] is the wire-level failure type
//! returned by [`crate::FrameTransport`] and [`crate::TransportFactory`]:
//! open failures, broken pipes, timeouts, EOF, framing violations.
//! [`SessionError`] is the user-facing failure type returned by
//! [`crate::Session::request`]; it discriminates a transport error from a
//! codec error so callers can pattern-match without parsing strings.
//!
//! Per the design plan, no implicit `From<TransportError>` bound is placed
//! on `Codec::Error`. Each service flattens `SessionError<C::Error>` into
//! its own service-wide error enum at the `Manager` public-API boundary.

use std::io;

use thiserror::Error;

/// Wire-level failure surfaced by a [`crate::FrameTransport`] or
/// [`crate::TransportFactory`].
#[derive(Debug, Error)]
pub enum TransportError {
    /// [`crate::TransportFactory::open`] failed before any frames flowed
    /// — e.g. the serial port doesn't exist, the UDP bind failed,
    /// permission denied.
    #[error("transport open failed: {0}")]
    Open(#[source] io::Error),

    /// I/O error during a `send_frame` or `recv_frame` after the
    /// transport was successfully opened — broken pipe, write error,
    /// non-timeout read error.
    #[error("transport I/O error: {0}")]
    Io(#[source] io::Error),

    /// Operation exceeded the configured timeout. The `Duration` is the
    /// timeout that fired, not the elapsed wall time.
    #[error("transport timed out after {0:?}")]
    Timeout(std::time::Duration),

    /// Read reached end-of-file before a complete frame was received.
    /// Distinct from [`TransportError::Io`] so callers can recognise a
    /// closed peer vs a transient I/O fault.
    #[error("transport reached EOF before a complete frame")]
    Eof,

    /// Frame violates the transport's framing rules — exceeded
    /// `max_frame_size` without a terminator, malformed datagram, etc.
    #[error("framing error: {0}")]
    Framing(String),

    /// The shared transport is currently reconnecting after a transient
    /// transport-loss event (USB drop / replug, cable jostle, peer
    /// reset). Returned from `Session::request` while the supervisor
    /// is mid-recovery so callers don't queue against the dying
    /// transport's command lock. The retry shape is caller-defined;
    /// the reconnect supervisor recovers the transport on its next
    /// attempt once the hardware reappears.
    ///
    /// `SharedTransport::acquire()` does **not** short-circuit on this
    /// state in the current implementation — the on-acquire eager
    /// reconnect path is a follow-up (see the plan doc's reconnect
    /// triggers table).
    #[error("transport is reconnecting")]
    Reconnecting,
}

/// User-facing failure for [`crate::Session::request`].
///
/// The `Codec` variant carries the codec's own error type, so a caller
/// can pattern-match on whether the wire failed or the bytes-to-typed
/// translation failed. [`SessionError`] derives `Error` so it composes
/// with `?` against any [`std::error::Error`] context.
#[derive(Debug, Error)]
pub enum SessionError<E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    /// Wire-level failure: open, read, write, timeout, EOF, framing.
    #[error(transparent)]
    Transport(#[from] TransportError),

    /// Codec-level failure: malformed response, mismatched checksum,
    /// or a frame the codec couldn't translate into a typed response.
    #[error(transparent)]
    Codec(E),

    /// The connection read [`crate::Codec::max_skip`] + 1 frames after
    /// sending the request and none of them satisfied
    /// [`crate::Codec::matches`]. The device either fell out of sync
    /// or the codec's `matches` predicate is wrong.
    #[error("skip budget exhausted after {0} non-matching frame(s)")]
    SkipExhausted(usize),
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[derive(Debug, Error)]
    #[error("stub codec error")]
    struct StubCodecError;

    #[test]
    fn from_transport_error_promotes_into_session_error() {
        let err: SessionError<StubCodecError> = TransportError::Eof.into();
        assert!(matches!(err, SessionError::Transport(TransportError::Eof)));
    }

    #[test]
    fn codec_error_does_not_implicitly_coerce_from_transport() {
        // Confirms the discriminated-union design: a codec error has to
        // be constructed explicitly. If a future change adds a blanket
        // `From<TransportError> for E` impl on someone's codec error,
        // this test still passes — it only documents the shape today.
        let err: SessionError<StubCodecError> = SessionError::Codec(StubCodecError);
        assert!(matches!(err, SessionError::Codec(_)));
    }

    #[test]
    fn display_passes_through_to_source() {
        let err: SessionError<StubCodecError> = SessionError::Codec(StubCodecError);
        assert_eq!(err.to_string(), "stub codec error");
    }

    #[test]
    fn transport_error_open_carries_io_source() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "no such device");
        let err = TransportError::Open(io_err);
        assert!(err.to_string().contains("transport open failed"));
    }
}

//! Per-open-transport request arbitration.
//!
//! [`Connection`] is the internal type that owns one boxed
//! [`crate::FrameTransport`] and one [`crate::Codec`], plus the command
//! lock that serialises request/response pairs across all callers of
//! the same open transport.
//!
//! It's exposed publicly because [`crate::Hooks::handshake`] receives
//! `&Connection<C>` so handshake commands run through the same request
//! arbitration as steady-state commands — `Session` and `WhileOpen` are
//! views of the same underlying `Arc<Connection<C>>`.
//!
//! Each connection optionally carries an `Arc<Notify>` (set by
//! [`crate::SharedTransport`] when it constructs the connection) that
//! fires once per `TransportError` observed in `request`. The
//! reconnect supervisor listens on this notify to react to mid-stream
//! transport loss — codec errors and skip-budget exhaustion do **not**
//! fire it, since those are protocol mismatches that a reconnect
//! cannot fix.

use std::fmt;
use std::sync::Arc;

use tokio::sync::{Mutex, Notify};
use tracing::trace;

use crate::codec::Codec;
use crate::error::SessionError;
use crate::transport::FrameTransport;

/// Maximum number of bytes rendered inside one wire-trace event. Bytes
/// past this point are summarised as `…[N more bytes]` so a misbehaving
/// codec or peer can't push multi-megabyte log lines. 256 bytes
/// comfortably covers every single-command frame in the five Alpaca
/// codecs that ship today (longest known: qhy-focuser's status JSON,
/// which lands well under).
const MAX_WIRE_TRACE_BYTES: usize = 256;

/// `Display` wrapper that renders a wire-byte slice safely for trace
/// logging. Printable ASCII (0x20..=0x7E, minus `\`) survives as-is so
/// operators can recognise commands like `:e1` or `{"cmd":"getstatus"}`;
/// every other byte renders as `\xNN`. This avoids two failure modes
/// from naive `format!("{:?}", bytes)`:
///
/// 1. **Terminal hijack.** A frame containing an ESC / CSI / BEL
///    sequence could colour the operator's terminal, ring the bell, or
///    in pathological cases relocate the cursor when they `tail -f` a
///    log. Escaping every non-printable byte to a literal `\xNN`
///    closes that hole.
/// 2. **Length blowup.** Capped at [`MAX_WIRE_TRACE_BYTES`] with a
///    summary tail. The full length is preserved in a separate
///    structured field on the `trace!` event for grep / aggregation.
struct DisplayWire<'a>(&'a [u8]);

impl fmt::Display for DisplayWire<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &b in self.0.iter().take(MAX_WIRE_TRACE_BYTES) {
            // Printable ASCII passes through; `\` is escaped so the
            // output is unambiguously parseable (a literal backslash
            // can't be mistaken for the start of a `\xNN` escape).
            if (0x20..=0x7E).contains(&b) && b != b'\\' {
                f.write_str(std::str::from_utf8(std::slice::from_ref(&b)).unwrap_or("?"))?;
            } else {
                write!(f, "\\x{b:02x}")?;
            }
        }
        if self.0.len() > MAX_WIRE_TRACE_BYTES {
            write!(
                f,
                "…[{} more bytes]",
                self.0.len().saturating_sub(MAX_WIRE_TRACE_BYTES)
            )?;
        }
        Ok(())
    }
}

/// One open transport + its codec + a command lock.
///
/// All wire I/O for a service goes through this single point. The mutex
/// guards the boxed transport so that concurrent callers
/// (handshake, foreground requests, the while-open poll task) take
/// turns end-to-end on the wire instead of interleaving bytes.
pub struct Connection<C: Codec> {
    transport: Mutex<Box<dyn FrameTransport>>,
    codec: C,
    /// Notify fired on every `TransportError` from `request`.
    /// `Some(_)` for every connection that `SharedTransport` itself
    /// builds — that includes the `LazyAcquire`-mode 0→1 cold-start
    /// path in `acquire()`, the `ServiceLifetime`-mode `start()`
    /// path, and the supervisor's `attempt_reconnect()`; all three
    /// attach the signal via `.with_reconnect_signal(...)` before
    /// running the handshake. The supervisor task itself is what
    /// listens for the notifications. `None` only for ad-hoc
    /// connections built via `Connection::new()` directly — in
    /// practice that's just in-crate unit tests; the wiring at
    /// every `SharedTransport` callsite always attaches the signal,
    /// so the `LazyAcquire` branch's lack of an active listener (the
    /// supervisor doesn't run until `start()` is called) means
    /// transport-error notifications in `LazyAcquire` mode no-op
    /// rather than waking anything — harmless, since `LazyAcquire`'s
    /// recovery model is "next acquire reopens".
    reconnect_signal: Option<Arc<Notify>>,
}

impl<C: Codec> Connection<C> {
    /// Create a connection that owns `transport` and uses `codec` for
    /// all frame translation. Internal; only [`crate::SharedTransport`]
    /// constructs these.
    pub(crate) fn new(transport: Box<dyn FrameTransport>, codec: C) -> Self {
        Self {
            transport: Mutex::new(transport),
            codec,
            reconnect_signal: None,
        }
    }

    /// Attach a reconnect signal. Called by
    /// [`crate::SharedTransport`] right after constructing a connection
    /// destined for the slot, before it's published to clients.
    pub(crate) fn with_reconnect_signal(mut self, signal: Arc<Notify>) -> Self {
        self.reconnect_signal = Some(signal);
        self
    }

    /// Send `cmd` and return the matching typed response.
    ///
    /// Holds the command lock for the entire request/response
    /// exchange: encode → `send_frame` → (read frames until one matches
    /// or `max_skip` is exhausted) → decode. The lock is released when
    /// this future completes (success or error).
    ///
    /// On a [`crate::TransportError`], also fires the attached
    /// `reconnect_signal` (if any). Codec errors and skip-budget
    /// exhaustion do not signal — those are protocol mismatches, not
    /// hardware loss.
    ///
    /// # Tracing
    ///
    /// Each `send_frame` / `recv_frame` round emits a `trace!` event
    /// with the wire bytes (escaped + length-capped via [`DisplayWire`])
    /// and the full byte count as a structured field. Disabled by
    /// default — enable per-target with
    /// `RUST_LOG=rusty_photon_shared_transport=trace` (or a finer
    /// filter) when debugging.
    ///
    /// # Security
    ///
    /// `Connection<C>` is generic over the codec; the trace events
    /// log raw wire bytes verbatim and **cannot redact
    /// protocol-specific sensitive content** (the layer doesn't know
    /// what the bytes mean). Operators enabling trace logging on a
    /// service whose codec carries credentials, PII, or other secrets
    /// own that disclosure. The `DisplayWire` formatter does escape
    /// non-printable bytes (so log-tail control sequences can't
    /// reach the terminal) and caps printed length, but those are
    /// log-safety guards, not content redaction.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Transport`] on a wire-level failure
    /// (which also signals the reconnect supervisor),
    /// [`SessionError::Codec`] when the response fails to decode, and
    /// [`SessionError::SkipExhausted`] when too many non-matching
    /// frames arrive.
    pub async fn request(&self, cmd: C::Command) -> Result<C::Response, SessionError<C::Error>> {
        let bytes = self.codec.encode(&cmd);
        let mut transport = self.transport.lock().await;
        trace!(
            len = bytes.len(),
            bytes = %DisplayWire(&bytes),
            "wire send"
        );
        match transport.send_frame(&bytes).await {
            Ok(()) => {}
            Err(e) => {
                self.signal_reconnect();
                return Err(SessionError::Transport(e));
            }
        }

        let mut buf = Vec::new();
        let budget = self.codec.max_skip();
        for skipped in 0..=budget {
            if let Err(e) = transport.recv_frame(&mut buf).await {
                self.signal_reconnect();
                return Err(SessionError::Transport(e));
            }
            trace!(
                len = buf.len(),
                skipped,
                bytes = %DisplayWire(&buf),
                "wire recv"
            );
            let resp = self.codec.decode(&buf).map_err(SessionError::Codec)?;
            if self.codec.matches(&cmd, &resp) {
                return Ok(resp);
            }
        }
        drop(transport);
        Err(SessionError::SkipExhausted(budget.saturating_add(1)))
    }

    fn signal_reconnect(&self) {
        if let Some(sig) = self.reconnect_signal.as_ref() {
            sig.notify_one();
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn display_wire_passes_printable_ascii_through_verbatim() {
        assert_eq!(format!("{}", DisplayWire(b"hello")), "hello");
        assert_eq!(format!("{}", DisplayWire(b":e1")), ":e1");
        assert_eq!(
            format!("{}", DisplayWire(b"{\"cmd\":\"getstatus\"}")),
            "{\"cmd\":\"getstatus\"}"
        );
    }

    #[test]
    fn display_wire_escapes_non_printable_bytes_as_hex() {
        // \r (0x0d) is a common frame terminator in the Sky-Watcher
        // motor protocol — must escape so a `tail -f` doesn't see a
        // bare carriage return.
        assert_eq!(format!("{}", DisplayWire(b":e1\r")), ":e1\\x0d");
        // Bell, ESC, CSI — the terminal-hijack vectors.
        assert_eq!(format!("{}", DisplayWire(&[0x07])), "\\x07");
        assert_eq!(
            format!("{}", DisplayWire(&[0x1b, b'[', b'2', b'J'])),
            "\\x1b[2J"
        );
    }

    #[test]
    fn display_wire_escapes_backslash_so_format_is_unambiguous() {
        // Without this, a literal `\` in the data would visually
        // ambiguate with the `\xNN` escape sequences.
        assert_eq!(format!("{}", DisplayWire(b"a\\b")), "a\\x5cb");
    }

    #[test]
    fn display_wire_truncates_at_cap_with_summary_tail() {
        let bytes = vec![b'a'; MAX_WIRE_TRACE_BYTES + 17];
        let s = format!("{}", DisplayWire(&bytes));
        assert!(s.starts_with(&"a".repeat(MAX_WIRE_TRACE_BYTES)));
        assert!(
            s.ends_with("[17 more bytes]"),
            "expected summary tail with extra-byte count, got: {s}"
        );
    }

    #[test]
    fn display_wire_at_cap_emits_no_tail() {
        let bytes = vec![b'a'; MAX_WIRE_TRACE_BYTES];
        let s = format!("{}", DisplayWire(&bytes));
        assert_eq!(s, "a".repeat(MAX_WIRE_TRACE_BYTES));
        assert!(!s.contains("more bytes"));
    }

    #[test]
    fn display_wire_handles_empty_slice() {
        assert_eq!(format!("{}", DisplayWire(b"")), "");
    }

    #[test]
    fn display_wire_does_not_crash_on_full_byte_range() {
        // Exercises every possible byte to confirm the formatter
        // never panics on weird inputs — important since this runs
        // inside a trace! call where a panic would be especially
        // surprising.
        let bytes: Vec<u8> = (0u8..=255).collect();
        let s = format!("{}", DisplayWire(&bytes));
        // Sanity-check a couple of representative escapes survived.
        assert!(s.contains("\\x00"));
        assert!(s.contains("\\xff"));
        assert!(s.contains('A'));
    }
}

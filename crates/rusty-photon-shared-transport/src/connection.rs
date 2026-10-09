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
//! fires once per `TransportError` `request` observes *on the wire*.
//! The reconnect supervisor listens on this notify to react to
//! mid-stream transport loss. Two kinds of failure deliberately do not
//! fire it: codec errors and skip-budget exhaustion, which are
//! protocol mismatches a reconnect cannot fix, and a request that
//! found the conduit already closed, which is where teardown leaves
//! things and not something to recover from. Of those, only the
//! closed-conduit case counts toward [`Connection::wire_failures`]:
//! that counter is about requests that never reached the device, and a
//! codec error or an exhausted skip budget means one did reach it and
//! answered.
//!
//! [`Connection::request_timed`] also reports *when* the exchange
//! crossed the host's side of the wire — see [`WireTiming`].
//!
//! # An exchange runs to completion
//!
//! Once a request has the command lock, its exchange runs on its own
//! task, to its reply, whether or not the caller is still waiting. A
//! caller can stop waiting at any point — an HTTP client hangs up and
//! the server drops its handler, a poll task is aborted — and the
//! protocols here carry no tag a later reader could use to recognise a
//! reply that was left unread. Were the exchange dropped with its
//! caller, its reply would answer the next request on the conduit, and
//! every reply after it would be one exchange late until a read timed
//! out or the conduit was reopened. Finishing the exchange keeps each
//! reply with the request that sent it, and keeps the device to one
//! command outstanding at a time. The cost is that the next request
//! waits for it: one round trip, or one read timeout if the reply never
//! comes — the same wait it has behind any request still in flight.
//!
//! A request that has not yet taken the lock sends nothing when its
//! caller goes away.

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Notify, OwnedMutexGuard};
use tokio::time::Instant;
use tracing::{debug, trace, Instrument};

use crate::codec::Codec;
use crate::error::{SessionError, TransportError};
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

/// When one request's frames crossed the host's side of the wire.
///
/// All three instants are on tokio's clock and are taken inside the
/// command lock, so time spent queued behind other callers is in none
/// of them:
///
/// * `sent_at` — immediately before the command frame is handed to
///   [`FrameTransport::send_frame`]. Nothing in this layer runs between
///   the stamp and the write, so a busy host can delay *when* a command
///   goes out without moving this away from it. What can still come
///   between them only makes the stamp early, never late: the runtime
///   may make the task wait for writability or for its cooperative
///   budget before the write, and the OS may preempt the thread.
/// * `written_at` — immediately after `send_frame` returned: the frame
///   had been handed to the OS. [`Self::send_gap`] is normally
///   microseconds; a larger one says the task was held up on the way
///   to the write, and by how much.
/// * `received_at` — immediately after the `recv_frame` that returned
///   the answering frame. The reply reached the host earlier than this
///   by however long the host took to notice it: the kernel has to
///   complete the transfer and wake the reader, the runtime has to
///   dispatch the readiness and run the task, and on a loaded host each
///   can wait tens of milliseconds.
///
/// So the device handled the command somewhere in
/// `[sent_at, received_at]`, whatever the host was doing — provided the
/// answering frame really is this command's reply. A protocol whose
/// replies carry no tag (the default [`Codec::matches`]) can be handed a
/// frame left over from an earlier exchange, such as one that arrived
/// after its request timed out; then the reply predates `sent_at`, and
/// [`Self::round_trip`] comes out shorter than the link can physically
/// manage.
///
/// Where inside the interval the device acts is a property of the
/// device and the link, not of this layer: a caller that has to date a
/// value the device latched picks its convention from the protocol it
/// speaks. The asymmetry above is what usually decides it — host load
/// lands on the reply's side of the exchange, so a device that latches
/// on receipt of the command is dated better by `sent_at` than by
/// `received_at` or by their midpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireTiming {
    /// Taken just before the command frame was handed to the transport.
    pub sent_at: Instant,
    /// Taken just after the transport accepted the command frame.
    pub written_at: Instant,
    /// Taken just after the answering frame was read.
    pub received_at: Instant,
}

impl WireTiming {
    /// `received_at − sent_at`: an upper bound on how far either end of
    /// the exchange can be from the instant the device handled the
    /// command.
    #[must_use]
    pub fn round_trip(&self) -> Duration {
        self.received_at.saturating_duration_since(self.sent_at)
    }

    /// `written_at − sent_at`: how long the task took to get the frame
    /// to the OS once it held the wire. Normally microseconds.
    #[must_use]
    pub fn send_gap(&self) -> Duration {
        self.written_at.saturating_duration_since(self.sent_at)
    }
}

/// One open transport + its codec + a command lock.
///
/// All wire I/O for a service goes through this single point. The mutex
/// guards the boxed transport so that concurrent callers
/// (handshake, foreground requests, the while-open poll task) take
/// turns end-to-end on the wire instead of interleaving bytes.
pub struct Connection<C: Codec> {
    /// `None` once [`Connection::close`] has run. Closing is explicit
    /// rather than left to the last `Arc<Connection<C>>` drop because
    /// an exclusive conduit — a serial port is one — can only be
    /// re-opened after the OS handle is gone, and the set of `Arc`
    /// holders at that moment (live `Session`s, an aborted `while_open`
    /// task) is not something the reconnect and shutdown paths can
    /// bound.
    ///
    /// Behind an `Arc` so an exchange's task can own the lock, and
    /// hold it to the end of the exchange after its caller has gone.
    transport: Arc<Mutex<Option<Box<dyn FrameTransport>>>>,
    codec: C,
    failures: WireFailures,
}

/// Where requests on one connection report failing on the wire. Cloned
/// into each exchange's task, which can outlive the request's caller.
#[derive(Clone)]
struct WireFailures {
    /// Notify fired on a `TransportError` from `request` that came off
    /// the wire. Not on one raised because the conduit was already
    /// closed: that is where teardown leaves things, and waking a
    /// supervisor to recover from a close someone asked for would have
    /// it tear down the conduit its own lifecycle just opened. Such a
    /// request is still counted as having failed — see
    /// [`Connection::wire_failures`] — because the caller asking
    /// whether its commands landed must still be told no.
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
    /// Count of requests that failed on the wire, i.e. every one that
    /// fired [`WireFailures::signal_reconnect`]. Lets a caller that ran
    /// commands on this connection ask afterwards whether they landed
    /// — which the hooks cannot say, because they stay best-effort
    /// about their own errors and log rather than propagate. The
    /// reconnect's safety replay is the one caller that needs to know,
    /// since a stop that did not land must not be reported as a
    /// recovered transport.
    ///
    /// This counter answers only half the question. It sees a request
    /// that never reached the device; it cannot see one the device
    /// *answered* — a refusal, or a reply that would not decode above
    /// this layer — because such a request completes like any other.
    /// That half is the hook's own verdict — see
    /// [`crate::StateAssertion`] — and the two are read together
    /// wherever it matters.
    wire_failures: Arc<AtomicU32>,
}

impl WireFailures {
    /// A request that found the conduit closed: counted, not signalled.
    fn count(&self) {
        self.wire_failures.fetch_add(1, Ordering::SeqCst);
    }

    /// A request that failed on the wire: counted, and the supervisor
    /// woken.
    fn signal_reconnect(&self) {
        self.count();
        if let Some(sig) = self.reconnect_signal.as_ref() {
            sig.notify_one();
        }
    }
}

impl<C: Codec> Connection<C> {
    /// Create a connection that owns `transport` and uses `codec` for
    /// all frame translation. Internal; only [`crate::SharedTransport`]
    /// constructs these.
    pub(crate) fn new(transport: Box<dyn FrameTransport>, codec: C) -> Self {
        Self {
            transport: Arc::new(Mutex::new(Some(transport))),
            codec,
            failures: WireFailures {
                reconnect_signal: None,
                wire_failures: Arc::new(AtomicU32::new(0)),
            },
        }
    }

    /// Close the underlying conduit now, without waiting for the last
    /// `Arc<Connection<C>>` to drop.
    ///
    /// Waits for an in-flight request to finish, which is bounded by
    /// the implementation's own I/O timeout — see the contract on
    /// [`FrameTransport`](crate::FrameTransport). Waiting is
    /// deliberate: abandoning the lock would leave the conduit open,
    /// and releasing it is the whole point.
    ///
    /// Takes the command lock, so an in-flight request finishes first;
    /// every request after this one fails with an I/O error naming the
    /// closed transport. Idempotent — closing twice is a no-op.
    ///
    /// This is what makes a same-port re-open safe. Dropping the last
    /// `Arc` would close the conduit too, but the reconnect and
    /// shutdown paths cannot prove they hold the last one, and on
    /// Windows the OS handle outlives the drop besides (see
    /// [`crate::transport::open_serial_port`]).
    pub(crate) async fn close(&self) {
        let mut guard = self.transport.lock().await;
        let closed = guard.take();
        let was_open = closed.is_some();

        // Drop the stream while still holding the lock. Taking it out
        // and dropping it afterwards would let a second caller see
        // `None`, conclude the conduit is released and ask the factory
        // for the port — while the first caller is still dropping the
        // stream. On Windows that is the race this whole path exists
        // to avoid, and `shutdown()` racing a reconnect is exactly the
        // pair that would hit it.
        drop(closed);
        drop(guard);

        if was_open {
            trace!("transport closed");
        }
    }

    /// Attach a reconnect signal. Called by
    /// [`crate::SharedTransport`] right after constructing a connection
    /// destined for the slot, before it's published to clients.
    pub(crate) fn with_reconnect_signal(mut self, signal: Arc<Notify>) -> Self {
        self.failures.reconnect_signal = Some(signal);
        self
    }

    /// Send `cmd` and return the matching typed response.
    ///
    /// Holds the command lock for the entire request/response
    /// exchange: encode → `send_frame` → (read frames until one matches
    /// or `max_skip` is exhausted) → decode. The lock is released when
    /// the exchange ends (success or error).
    ///
    /// # Cancellation
    ///
    /// Dropping this future before it has the command lock sends
    /// nothing. Dropping it after does not stop the exchange: that runs
    /// on its own task to its reply, which it reads and discards, so the
    /// next request on the conduit reads its own. See the [module
    /// documentation](self#an-exchange-runs-to-completion) for why.
    ///
    /// On a [`crate::TransportError`] raised by the wire, also fires
    /// the attached `reconnect_signal` (if any). Three failures do not
    /// signal: codec errors and skip-budget exhaustion, which are
    /// protocol mismatches rather than hardware loss, and a request
    /// that found the conduit already closed, which is a teardown
    /// someone asked for rather than one to recover from. Of the
    /// three, only the last counts toward
    /// [`Connection::wire_failures`] — the other two mean the device
    /// did answer, and that counter is about requests that never
    /// reached it.
    ///
    /// # Tracing
    ///
    /// Each `send_frame` / `recv_frame` round emits a `trace!` event
    /// with the wire bytes (escaped + length-capped via [`DisplayWire`])
    /// and the full byte count as a structured field; `wire recv` also
    /// carries the `rtt` since the command frame went out. Disabled by
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
        self.request_timed(cmd).await.map(|(resp, _)| resp)
    }

    /// [`Self::request`], plus when the exchange crossed the wire.
    ///
    /// The [`WireTiming`] is that of the one command frame and of the
    /// frame that answered it; it ends at the answer, not at any frame
    /// skipped under [`Codec::max_skip`]. Each `wire recv` trace event,
    /// a skipped frame's included, carries the `rtt` since the command
    /// frame went out.
    ///
    /// # Errors
    ///
    /// The [`Self::request`] failures, unchanged.
    pub async fn request_timed(
        &self,
        cmd: C::Command,
    ) -> Result<(C::Response, WireTiming), SessionError<C::Error>> {
        let bytes = self.codec.encode(&cmd);
        let exchange = Exchange {
            // Taken here, by the caller, so that a caller which goes
            // away while it queues for the wire has sent nothing.
            transport: Arc::clone(&self.transport).lock_owned().await,
            codec: self.codec.clone(),
            failures: self.failures.clone(),
        }
        .run(cmd, bytes);
        // The span keeps the exchange's trace events with the request
        // that started it, which is also what names an exchange that
        // ran on after its caller left.
        let task = tokio::spawn(exchange.in_current_span());
        let waiting = CallerWaiting { answered: false };
        let outcome = task.await;
        waiting.answered();
        match outcome {
            Ok(result) => result,
            // A panic in the exchange (in the codec, say) reaches the
            // caller, as it did when the exchange ran inline. Nothing
            // else ends the task while a caller still waits on it: only
            // a runtime shutdown cancels it, and that drops the caller
            // too.
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        }
    }

    /// How many requests on this connection have failed on the wire.
    /// Counted even when no signal is attached, so the value means
    /// "did not reach the device", not "woke the supervisor".
    pub(crate) fn wire_failures(&self) -> u32 {
        self.failures.wire_failures.load(Ordering::SeqCst)
    }
}

/// One request/response exchange, holding the command lock, on a task
/// of its own so that it ends at its reply rather than when its caller
/// stops waiting. See [`Connection::request_timed`].
struct Exchange<C: Codec> {
    transport: OwnedMutexGuard<Option<Box<dyn FrameTransport>>>,
    codec: C,
    failures: WireFailures,
}

impl<C: Codec> Exchange<C> {
    async fn run(
        self,
        cmd: C::Command,
        bytes: Vec<u8>,
    ) -> Result<(C::Response, WireTiming), SessionError<C::Error>> {
        let Self {
            transport: mut guard,
            codec,
            failures,
        } = self;
        // Reached only by a caller that raced `close` — the reconnect
        // and shutdown paths both quiesce their callers first.
        let Some(transport) = guard.as_mut() else {
            // Counted, not signalled: a closed conduit is where
            // teardown leaves things, so it must not wake the
            // supervisor — but the command did not reach the device,
            // and a caller asking afterwards whether its commands
            // landed has to be told no.
            failures.count();
            return Err(SessionError::Transport(TransportError::Io(
                io::Error::other("transport closed"),
            )));
        };
        trace!(
            len = bytes.len(),
            bytes = %DisplayWire(&bytes),
            "wire send"
        );
        // After the trace event, not before it: with wire tracing on,
        // emitting the event is synchronous I/O that would otherwise
        // sit between the stamp and the write.
        let sent_at = Instant::now();
        match transport.send_frame(&bytes).await {
            Ok(()) => {}
            Err(e) => {
                failures.signal_reconnect();
                return Err(SessionError::Transport(e));
            }
        }
        let written_at = Instant::now();

        let mut buf = Vec::new();
        let budget = codec.max_skip();
        for skipped in 0..=budget {
            if let Err(e) = transport.recv_frame(&mut buf).await {
                failures.signal_reconnect();
                return Err(SessionError::Transport(e));
            }
            let timing = WireTiming {
                sent_at,
                written_at,
                received_at: Instant::now(),
            };
            let rtt = timing.round_trip();
            trace!(
                len = buf.len(),
                skipped,
                rtt = ?rtt,
                bytes = %DisplayWire(&buf),
                "wire recv"
            );
            let resp = codec.decode(&buf).map_err(SessionError::Codec)?;
            if codec.matches(&cmd, &resp) {
                return Ok((resp, timing));
            }
        }
        drop(guard);
        Err(SessionError::SkipExhausted(budget.saturating_add(1)))
    }
}

/// Records, from the caller's side, a caller that stopped waiting while
/// its exchange was on the wire. The exchange runs on regardless; this
/// only says so in the log.
struct CallerWaiting {
    answered: bool,
}

impl CallerWaiting {
    fn answered(mut self) {
        self.answered = true;
    }
}

impl Drop for CallerWaiting {
    fn drop(&mut self) {
        if !self.answered {
            debug!("the caller stopped waiting mid-exchange; the exchange runs on to its reply");
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

    // -----------------------------------------------------------------
    // request() against a closed or unresponsive transport
    // -----------------------------------------------------------------

    /// Echoes whatever was sent on the next `recv_frame`.
    struct EchoTransport(Option<Vec<u8>>);

    #[async_trait::async_trait]
    impl FrameTransport for EchoTransport {
        async fn send_frame(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
            self.0 = Some(bytes.to_vec());
            Ok(())
        }

        async fn recv_frame(&mut self, buf: &mut Vec<u8>) -> Result<(), TransportError> {
            buf.clear();
            match self.0.take() {
                Some(sent) => {
                    buf.extend_from_slice(&sent);
                    Ok(())
                }
                None => Err(TransportError::Eof),
            }
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("stub codec error")]
    struct StubCodecError;

    /// Identity codec. `MATCHES` decides whether a decoded response is
    /// taken as the answer to the command, which is what drives the
    /// skip budget.
    #[derive(Clone)]
    struct StubCodec<const MATCHES: bool>;

    impl<const MATCHES: bool> Codec for StubCodec<MATCHES> {
        type Command = Vec<u8>;
        type Response = Vec<u8>;
        type Error = StubCodecError;

        fn encode(&self, cmd: &Self::Command) -> Vec<u8> {
            cmd.clone()
        }

        fn decode(&self, bytes: &[u8]) -> Result<Self::Response, Self::Error> {
            Ok(bytes.to_vec())
        }

        fn matches(&self, _cmd: &Self::Command, _resp: &Self::Response) -> bool {
            MATCHES
        }
    }

    fn echo_connection<const MATCHES: bool>() -> Connection<StubCodec<MATCHES>> {
        Connection::new(Box::new(EchoTransport(None)), StubCodec::<MATCHES>)
    }

    #[tokio::test]
    async fn request_after_close_reports_the_closed_transport() {
        // The contract `close` documents: the conduit is gone, and a
        // caller that raced it is told so rather than talking to a
        // transport the reconnect path believes it has released.
        let conn = echo_connection::<true>();
        conn.request(b"ping".to_vec()).await.unwrap();

        conn.close().await;

        let err = conn.request(b"ping".to_vec()).await.unwrap_err();
        assert!(
            err.to_string().contains("transport closed"),
            "expected the closed-transport error, got: {err}"
        );

        // Counted as a failure, not just reported as one. This is the
        // path a safety hook takes when a 1→0 lands mid-reconnect, and
        // the reconnect decides whether that state is still owed by
        // asking the connection whether its commands landed.
        assert_eq!(
            conn.wire_failures(),
            1,
            "a command against a closed conduit did not reach the device"
        );
    }

    #[tokio::test]
    async fn close_is_idempotent() {
        let conn = echo_connection::<true>();
        conn.close().await;
        conn.close().await;
        conn.request(b"ping".to_vec()).await.unwrap_err();
    }

    // -----------------------------------------------------------------
    // request_timed(): when the exchange crossed the wire
    // -----------------------------------------------------------------

    /// How long a reply takes in the timing tests — the 41 ms a stalled
    /// host took to notice a reply on the rig in issue #1371.
    const REPLY_DELAY: Duration = Duration::from_millis(41);

    /// How long the write takes in the test that holds one up.
    const SEND_DELAY: Duration = Duration::from_millis(3);

    /// [`EchoTransport`] whose write takes `send_delay` and whose echo
    /// takes `reply_delay` to come back, the way they do when the host
    /// is slow to get the frame out or to notice the reply.
    struct SlowEchoTransport {
        inner: EchoTransport,
        send_delay: Duration,
        reply_delay: Duration,
    }

    #[async_trait::async_trait]
    impl FrameTransport for SlowEchoTransport {
        async fn send_frame(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
            tokio::time::sleep(self.send_delay).await;
            self.inner.send_frame(bytes).await
        }

        async fn recv_frame(&mut self, buf: &mut Vec<u8>) -> Result<(), TransportError> {
            tokio::time::sleep(self.reply_delay).await;
            self.inner.recv_frame(buf).await
        }
    }

    fn slow_echo_connection(
        send_delay: Duration,
        reply_delay: Duration,
    ) -> Connection<StubCodec<true>> {
        let transport = SlowEchoTransport {
            inner: EchoTransport(None),
            send_delay,
            reply_delay,
        };
        Connection::new(Box::new(transport), StubCodec::<true>)
    }

    #[tokio::test(start_paused = true)]
    async fn received_at_is_taken_when_the_answer_is_read() {
        let conn = slow_echo_connection(Duration::ZERO, REPLY_DELAY);
        let before = Instant::now();

        let (resp, timing) = conn.request_timed(b"ping".to_vec()).await.unwrap();

        assert_eq!(resp, b"ping");
        assert_eq!(
            timing.received_at,
            before.checked_add(REPLY_DELAY).unwrap(),
            "a reply the host is slow to notice is late in received_at"
        );
        assert_eq!(timing.round_trip(), REPLY_DELAY);
    }

    #[tokio::test(start_paused = true)]
    async fn sent_at_is_taken_before_the_write_and_written_at_after_it() {
        let conn = slow_echo_connection(SEND_DELAY, Duration::ZERO);
        let before = Instant::now();

        let (_, timing) = conn.request_timed(b"ping".to_vec()).await.unwrap();

        assert_eq!(
            timing.sent_at, before,
            "sent_at precedes the write, so a slow write cannot make it late"
        );
        assert_eq!(
            timing.written_at,
            before.checked_add(SEND_DELAY).unwrap(),
            "written_at follows the write"
        );
        assert_eq!(timing.send_gap(), SEND_DELAY);
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_queued_behind_another_is_stamped_when_it_reaches_the_wire() {
        // The second caller spends a whole exchange waiting for the
        // command lock. That wait is not wire time: its stamp has to
        // say when its own frame went out, or a sample it carries
        // would be dated an exchange too early.
        let conn = slow_echo_connection(Duration::ZERO, REPLY_DELAY);
        let before = Instant::now();

        let (one, two) = tokio::join!(
            conn.request_timed(b"one".to_vec()),
            conn.request_timed(b"two".to_vec()),
        );
        let mut timings = [one.unwrap().1, two.unwrap().1];
        timings.sort_by_key(|t| t.sent_at);
        let [first, queued] = timings;

        assert_eq!(first.sent_at, before);
        assert_eq!(
            queued.sent_at, first.received_at,
            "the lock wait is not in the stamp"
        );
        assert_eq!(
            queued.round_trip(),
            REPLY_DELAY,
            "the queued request's round trip is its own exchange"
        );
    }

    /// Replies with `frames` in order, each `delay` after the last, and
    /// ignores what is sent.
    struct ScriptedReplies {
        frames: std::collections::VecDeque<Vec<u8>>,
        delay: Duration,
    }

    #[async_trait::async_trait]
    impl FrameTransport for ScriptedReplies {
        async fn send_frame(&mut self, _bytes: &[u8]) -> Result<(), TransportError> {
            Ok(())
        }

        async fn recv_frame(&mut self, buf: &mut Vec<u8>) -> Result<(), TransportError> {
            tokio::time::sleep(self.delay).await;
            buf.clear();
            buf.extend(self.frames.pop_front().ok_or(TransportError::Eof)?);
            Ok(())
        }
    }

    /// Takes only an `ok` frame as the answer, skipping up to one other
    /// — the shape of a protocol that pushes unsolicited frames.
    #[derive(Clone)]
    struct AnswerIsOk;

    impl Codec for AnswerIsOk {
        type Command = Vec<u8>;
        type Response = Vec<u8>;
        type Error = StubCodecError;

        fn encode(&self, cmd: &Self::Command) -> Vec<u8> {
            cmd.clone()
        }

        fn decode(&self, bytes: &[u8]) -> Result<Self::Response, Self::Error> {
            Ok(bytes.to_vec())
        }

        fn matches(&self, _cmd: &Self::Command, resp: &Self::Response) -> bool {
            resp == b"ok"
        }

        fn max_skip(&self) -> usize {
            1
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_timing_ends_at_the_answer_not_at_a_skipped_frame() {
        let transport = ScriptedReplies {
            frames: [b"unsolicited".to_vec(), b"ok".to_vec()].into(),
            delay: REPLY_DELAY,
        };
        let conn = Connection::new(Box::new(transport), AnswerIsOk);
        let before = Instant::now();

        let (resp, timing) = conn.request_timed(b"ask".to_vec()).await.unwrap();

        assert_eq!(resp, b"ok");
        assert_eq!(timing.sent_at, before);
        assert_eq!(
            timing.round_trip(),
            REPLY_DELAY.saturating_mul(2),
            "the timing ends at the answering frame, not at the skipped one"
        );
    }

    #[tokio::test]
    async fn request_reports_the_skip_budget_when_nothing_matches() {
        // A codec whose `matches` never fires exhausts the default
        // budget of zero skips: one frame read, none accepted.
        let conn = echo_connection::<false>();

        let err = conn.request(b"ping".to_vec()).await.unwrap_err();
        match err {
            SessionError::SkipExhausted(n) => assert_eq!(n, 1, "one frame read and rejected"),
            other => panic!("expected SkipExhausted, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // A caller that stops waiting mid-exchange
    // -----------------------------------------------------------------

    /// A device on a line. Each frame written queues its answer, `re:`
    /// and the frame, the moment it is written; each read and each
    /// write waits until the test releases it. A read its caller
    /// abandons therefore leaves its answer on the line for whoever
    /// reads next, the way a serial port does.
    struct Line {
        answers: std::sync::Mutex<std::collections::VecDeque<Vec<u8>>>,
        written: std::sync::Mutex<Vec<Vec<u8>>>,
        reads: tokio::sync::Semaphore,
        writes: tokio::sync::Semaphore,
        reading: Notify,
        writing: Notify,
    }

    impl Line {
        /// Reads wait to be released; writes go straight out.
        fn new() -> Arc<Self> {
            Self::with_writes(tokio::sync::Semaphore::MAX_PERMITS)
        }

        /// Reads wait to be released, and only `writes` writes go out
        /// before the test releases more.
        fn with_writes(writes: usize) -> Arc<Self> {
            Arc::new(Self {
                answers: std::sync::Mutex::default(),
                written: std::sync::Mutex::default(),
                reads: tokio::sync::Semaphore::new(0),
                writes: tokio::sync::Semaphore::new(writes),
                reading: Notify::new(),
                writing: Notify::new(),
            })
        }

        fn written(&self) -> Vec<Vec<u8>> {
            self.written.lock().unwrap().clone()
        }

        fn unread(&self) -> usize {
            self.answers.lock().unwrap().len()
        }
    }

    struct LineTransport(Arc<Line>);

    #[async_trait::async_trait]
    impl FrameTransport for LineTransport {
        async fn send_frame(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
            self.0.writing.notify_one();
            self.0.writes.acquire().await.unwrap().forget();
            self.0.written.lock().unwrap().push(bytes.to_vec());
            let mut answer = b"re:".to_vec();
            answer.extend_from_slice(bytes);
            self.0.answers.lock().unwrap().push_back(answer);
            Ok(())
        }

        async fn recv_frame(&mut self, buf: &mut Vec<u8>) -> Result<(), TransportError> {
            self.0.reading.notify_one();
            self.0.reads.acquire().await.unwrap().forget();
            buf.clear();
            buf.extend(
                self.0
                    .answers
                    .lock()
                    .unwrap()
                    .pop_front()
                    .ok_or(TransportError::Eof)?,
            );
            Ok(())
        }
    }

    fn line_connection(line: &Arc<Line>) -> Arc<Connection<StubCodec<true>>> {
        Arc::new(Connection::new(
            Box::new(LineTransport(Arc::clone(line))),
            StubCodec::<true>,
        ))
    }

    /// Drive `request` until `reached` fires, then drop it: a caller
    /// that stops waiting at that point of its exchange.
    async fn abandon_at<F: std::future::Future>(request: F, reached: &Notify) {
        tokio::select! {
            _ = request => panic!("the request finished before it was abandoned"),
            () = reached.notified() => {}
        }
    }

    #[tokio::test]
    async fn a_caller_that_stops_waiting_for_its_reply_leaves_the_next_request_its_own() {
        // The rig's failure: an HTTP client hung up while its frame was
        // on the wire, the server dropped the handler, and the reply
        // went to the next request instead — then every reply after it
        // was one exchange late.
        let line = Line::new();
        let conn = line_connection(&line);
        abandon_at(conn.request(b"one".to_vec()), &line.reading).await;

        line.reads.add_permits(2);
        let reply = conn.request(b"two".to_vec()).await.unwrap();

        assert_eq!(reply, b"re:two", "the next request reads its own reply");
        assert_eq!(line.unread(), 0, "the abandoned reply was read, not left");
    }

    #[tokio::test]
    async fn a_caller_that_stops_waiting_while_its_frame_goes_out_still_sends_it_whole() {
        // A frame abandoned part-way through its write would reach the
        // device torn, and whether a device answers a torn frame is
        // anyone's guess.
        let line = Line::with_writes(0);
        let conn = line_connection(&line);
        abandon_at(conn.request(b"one".to_vec()), &line.writing).await;

        line.writes.add_permits(2);
        line.reads.add_permits(2);
        let reply = conn.request(b"two".to_vec()).await.unwrap();

        assert_eq!(line.written(), [b"one".to_vec(), b"two".to_vec()]);
        assert_eq!(reply, b"re:two");
    }

    #[tokio::test]
    async fn a_caller_that_stops_waiting_for_the_wire_sends_nothing() {
        // The lock is taken by the caller, so one that goes away while
        // it queues leaves no command behind it — a client that hung up
        // on a cover move does not move the cover later.
        let line = Line::new();
        let conn = line_connection(&line);
        let first = tokio::spawn({
            let conn = Arc::clone(&conn);
            async move { conn.request(b"one".to_vec()).await }
        });
        line.reading.notified().await;

        let queued = conn.request(b"two".to_vec());
        tokio::select! {
            biased;
            _ = queued => panic!("the queued request ran while the first held the wire"),
            () = tokio::task::yield_now() => {}
        }
        line.reads.add_permits(1);

        assert_eq!(first.await.unwrap().unwrap(), b"re:one");
        assert_eq!(
            line.written(),
            [b"one".to_vec()],
            "the queued frame never went out"
        );
    }

    /// Panics on decode, as a codec with a bug might.
    #[derive(Clone)]
    struct PanicsOnDecode;

    impl Codec for PanicsOnDecode {
        type Command = Vec<u8>;
        type Response = Vec<u8>;
        type Error = StubCodecError;

        fn encode(&self, cmd: &Self::Command) -> Vec<u8> {
            cmd.clone()
        }

        #[expect(
            clippy::panic_in_result_fn,
            reason = "a codec that panics is the behaviour under test"
        )]
        fn decode(&self, _bytes: &[u8]) -> Result<Self::Response, Self::Error> {
            panic!("the codec panicked");
        }
    }

    #[tokio::test]
    #[should_panic(expected = "the codec panicked")]
    async fn a_panic_in_the_exchange_reaches_the_caller() {
        let conn = Connection::new(Box::new(EchoTransport(None)), PanicsOnDecode);
        let _ = conn.request(b"ping".to_vec()).await;
    }
}

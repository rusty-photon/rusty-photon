//! Frame-oriented transport abstractions.
//!
//! [`FrameTransport`] is the low-level interface a single open conduit
//! satisfies: send one frame, receive one frame. Framing (terminator
//! buffering for serial, datagram boundaries for UDP) lives on the
//! implementor. The shared crate provides two ready-made implementations
//! ([`SerialFrameTransport`] for any `AsyncRead + AsyncWrite + Unpin`
//! stream, [`UdpFrameTransport`] for `tokio::net::UdpSocket`) and services
//! plug in additional ones as needed.
//!
//! [`TransportFactory`] is the "open me a transport" trait. The
//! [`SharedTransport`] core holds a factory and calls `open()` exactly
//! once per 0→1 connect transition.
//!
//! [`SharedTransport`]: crate::SharedTransport

use std::io;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UdpSocket;
use tokio::time::timeout;

use crate::error::TransportError;

/// One open send/receive conduit.
///
/// All frame-by-frame I/O lives behind this trait. The connection layer
/// holds a [`Box<dyn FrameTransport>`] under a mutex (the request
/// arbitration lock); `&mut self` is therefore sufficient and no
/// internal locking is required.
#[async_trait]
pub trait FrameTransport: Send {
    /// Send one whole frame.
    ///
    /// For [`SerialFrameTransport`], writes `bytes` verbatim with no
    /// explicit flush (see [`SerialFrameTransport::send_frame`] for
    /// why) — any in-frame terminator the protocol requires is the
    /// caller's responsibility. For [`UdpFrameTransport`], emits
    /// exactly one `send` call (one datagram on the wire). The
    /// bytes-on-the-wire are the same as the bytes passed in.
    async fn send_frame(&mut self, bytes: &[u8]) -> Result<(), TransportError>;

    /// Receive one whole frame, overwriting `buf`.
    ///
    /// For [`SerialFrameTransport`], reads bytes until the configured
    /// terminator is seen; the terminator is **included** in the result
    /// (this matches what `qhy-focuser`'s JSON parser, the
    /// `skywatcher-motor-protocol` codec, and the
    /// `pa-falcon-rotator`/`ppba-driver` ASCII echoes all expect). For
    /// [`UdpFrameTransport`], reads exactly one datagram; the buffer is
    /// resized to the datagram's length.
    async fn recv_frame(&mut self, buf: &mut Vec<u8>) -> Result<(), TransportError>;
}

/// Constructs a fresh [`FrameTransport`] each time the 0→1 connect
/// transition fires.
///
/// The factory carries the configuration — port path, baud rate, peer
/// address, etc. — captured at service-startup time. Per-call
/// parameters belong on the [`crate::Codec`] or on hooks.
#[async_trait]
pub trait TransportFactory: Send + Sync + 'static {
    /// Open the underlying device and return a ready-to-use transport.
    ///
    /// On error, no resources are leaked: the transport core leaves
    /// `count` and `available` untouched and the caller sees an `Err`
    /// from [`crate::SharedTransport::acquire`].
    async fn open(&self) -> Result<Box<dyn FrameTransport>, TransportError>;
}

/// Default I/O timeout applied when a transport is built without one.
///
/// Five seconds matches the existing per-service defaults
/// (`qhy-focuser`, `ppba-driver`, SAG-GTI all configure this in their
/// `Config`); centralising the default here lets services that don't
/// care use [`SerialFrameTransport::new`] / [`UdpFrameTransport::new`]
/// without specifying timeouts.
pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Classify an [`io::Error`] surfaced by a wrapped stream / socket into
/// the right [`TransportError`] variant.
///
/// Maps [`io::ErrorKind::TimedOut`] to [`TransportError::Timeout`] so
/// any underlying timeout (e.g. a port-level `VTIME` set on a serial
/// driver, an OS recv timer, a future async runtime's deadline) is
/// classified as a timeout — not collapsed into the generic
/// [`TransportError::Io`] bucket that other I/O errors land in. The
/// reported `Duration` is the transport's own configured timeout
/// (`read_timeout` for reads, `write_timeout` for writes), since the
/// underlying timer's actual duration isn't recoverable from
/// `io::Error`; callers branch on the variant rather than the
/// duration, so this is honest enough.
///
/// All other [`io::Error`] kinds (`BrokenPipe`, `ConnectionReset`, write
/// errors, etc.) become [`TransportError::Io`] verbatim.
fn classify_io_error(e: io::Error, on_timeout: Duration) -> TransportError {
    if e.kind() == io::ErrorKind::TimedOut {
        TransportError::Timeout(on_timeout)
    } else {
        TransportError::Io(e)
    }
}

/// Generic serial-stream frame transport.
///
/// Wraps any `AsyncRead + AsyncWrite + Unpin + Send` (most commonly
/// `tokio_serial::SerialStream`) in a [`BufReader`] for efficient
/// read-until-terminator handling. Constructed via a factory that
/// owns the per-call configuration (port path, baud rate, terminator
/// byte, max frame size).
///
/// Writes go straight to the wrapped stream with no [`BufWriter`]
/// layer and no explicit flush — see the
/// [`send_frame`](Self::send_frame) impl below for why.
///
/// [`BufWriter`]: tokio::io::BufWriter
pub struct SerialFrameTransport<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    stream: BufReader<S>,
    terminator: u8,
    max_frame_size: usize,
    read_timeout: Duration,
    write_timeout: Duration,
}

impl<S> SerialFrameTransport<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    /// Wrap `stream` with `terminator`-based framing and a frame-size
    /// guard. Timeouts default to [`DEFAULT_IO_TIMEOUT`]; override with
    /// [`with_read_timeout`](Self::with_read_timeout) /
    /// [`with_write_timeout`](Self::with_write_timeout).
    pub fn new(stream: S, terminator: u8, max_frame_size: usize) -> Self {
        Self {
            stream: BufReader::new(stream),
            terminator,
            max_frame_size,
            read_timeout: DEFAULT_IO_TIMEOUT,
            write_timeout: DEFAULT_IO_TIMEOUT,
        }
    }

    /// Override the per-`recv_frame` timeout. Default is
    /// [`DEFAULT_IO_TIMEOUT`].
    #[must_use]
    pub const fn with_read_timeout(mut self, d: Duration) -> Self {
        self.read_timeout = d;
        self
    }

    /// Override the per-`send_frame` timeout. Default is
    /// [`DEFAULT_IO_TIMEOUT`].
    #[must_use]
    pub const fn with_write_timeout(mut self, d: Duration) -> Self {
        self.write_timeout = d;
        self
    }

    /// Read into `buf` until the terminator is seen, EOF, or the
    /// in-progress frame would exceed `max_frame_size`.
    ///
    /// The size check fires **during** the read by consuming the
    /// `BufReader`'s buffered chunks incrementally rather than calling
    /// `read_until` (which has no internal bound). A peer that streams
    /// indefinitely without a terminator therefore errors out as soon
    /// as the would-be frame crosses `max_frame_size`, instead of
    /// growing `buf` unboundedly first.
    async fn read_frame_bounded(&mut self, buf: &mut Vec<u8>) -> Result<(), TransportError> {
        loop {
            let chunk = self
                .stream
                .fill_buf()
                .await
                .map_err(|e| classify_io_error(e, self.read_timeout))?;
            if chunk.is_empty() {
                if buf.is_empty() {
                    return Err(TransportError::Eof);
                }
                return Err(TransportError::Framing(format!(
                    "stream ended without terminator after {} bytes",
                    buf.len()
                )));
            }

            let budget = self.max_frame_size.saturating_sub(buf.len());
            let scan_end = chunk.len().min(budget);
            let terminator_pos = chunk
                .iter()
                .take(scan_end)
                .position(|&b| b == self.terminator);

            if let Some(pos) = terminator_pos {
                // `pos` indexes into `chunk`, so `..=pos` is in bounds.
                let Some(frame) = chunk.get(..=pos) else {
                    return Err(TransportError::Framing(
                        "terminator position out of bounds".to_string(),
                    ));
                };
                let n = frame.len();
                buf.extend_from_slice(frame);
                self.stream.consume(n);
                return Ok(());
            }

            if scan_end < chunk.len() {
                // Remaining budget exhausted with no terminator in
                // sight — the frame is already over the limit.
                return Err(TransportError::Framing(format!(
                    "frame exceeded max size {} bytes without terminator",
                    self.max_frame_size
                )));
            }

            let consumed = chunk.len();
            buf.extend_from_slice(chunk);
            self.stream.consume(consumed);
        }
    }
}

#[async_trait]
impl<S> FrameTransport for SerialFrameTransport<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    async fn send_frame(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        // Deliberately no `.flush()` after `write_all`: on Unix,
        // `tokio_serial::SerialStream::poll_flush` issues a `tcdrain(2)`
        // ioctl that blocks until the peer's UART hardware confirms
        // transmission — a block that happens synchronously inside a
        // single `poll()` call, so `tokio::time::timeout` below cannot
        // preempt it once entered. A peer that enumerates but never
        // services its endpoints (e.g. a bus-powered USB-CDC device
        // whose firmware is dead) wedges the write forever instead of
        // surfacing the honest `TransportError::Timeout` that drives
        // the reconnect/restart ladder. `write_all` alone hands the
        // bytes to the kernel's non-blocking write path; a
        // non-responding peer is instead caught by `recv_frame`'s
        // bounded read timeout.
        match timeout(self.write_timeout, self.stream.get_mut().write_all(bytes)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(classify_io_error(e, self.write_timeout)),
            Err(_) => Err(TransportError::Timeout(self.write_timeout)),
        }
    }

    async fn recv_frame(&mut self, buf: &mut Vec<u8>) -> Result<(), TransportError> {
        buf.clear();
        match timeout(self.read_timeout, self.read_frame_bounded(buf)).await {
            Ok(result) => result,
            Err(_) => Err(TransportError::Timeout(self.read_timeout)),
        }
    }
}

/// UDP-datagram frame transport.
///
/// Backed by a `connect()`-ed [`UdpSocket`] so `send`/`recv` are
/// peer-bound. Each [`FrameTransport::send_frame`] is one `send` call;
/// each [`FrameTransport::recv_frame`] is one `recv` call. Datagram
/// boundaries are preserved by construction — this is the whole reason
/// the trait is frame-oriented instead of `AsyncRead`/`AsyncWrite`-based.
pub struct UdpFrameTransport {
    socket: UdpSocket,
    max_frame_size: usize,
    read_timeout: Duration,
    write_timeout: Duration,
}

impl UdpFrameTransport {
    /// Wrap an already-`connect()`-ed [`UdpSocket`].
    ///
    /// The caller is responsible for binding to the correct local
    /// address and calling `socket.connect(peer).await` before handing
    /// the socket over; this constructor is intentionally low-level so
    /// services can apply their own bind-address rules
    /// (e.g. SAG-GTI requires binding inside the mount's subnet).
    pub const fn new(socket: UdpSocket, max_frame_size: usize) -> Self {
        Self {
            socket,
            max_frame_size,
            read_timeout: DEFAULT_IO_TIMEOUT,
            write_timeout: DEFAULT_IO_TIMEOUT,
        }
    }

    /// Override the per-`recv_frame` timeout.
    #[must_use]
    pub const fn with_read_timeout(mut self, d: Duration) -> Self {
        self.read_timeout = d;
        self
    }

    /// Override the per-`send_frame` timeout.
    #[must_use]
    pub const fn with_write_timeout(mut self, d: Duration) -> Self {
        self.write_timeout = d;
        self
    }
}

#[async_trait]
impl FrameTransport for UdpFrameTransport {
    async fn send_frame(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        if bytes.len() > self.max_frame_size {
            return Err(TransportError::Framing(format!(
                "datagram exceeds max size {} (got {})",
                self.max_frame_size,
                bytes.len()
            )));
        }
        match timeout(self.write_timeout, self.socket.send(bytes)).await {
            Ok(Ok(sent)) if sent == bytes.len() => Ok(()),
            Ok(Ok(short)) => Err(TransportError::Io(io::Error::other(format!(
                "udp send wrote {short} of {} bytes",
                bytes.len()
            )))),
            Ok(Err(e)) => Err(classify_io_error(e, self.write_timeout)),
            Err(_) => Err(TransportError::Timeout(self.write_timeout)),
        }
    }

    async fn recv_frame(&mut self, buf: &mut Vec<u8>) -> Result<(), TransportError> {
        buf.clear();
        buf.resize(self.max_frame_size, 0);
        match timeout(self.read_timeout, self.socket.recv(buf)).await {
            Ok(Ok(n)) => {
                buf.truncate(n);
                Ok(())
            }
            Ok(Err(e)) => {
                buf.clear();
                Err(classify_io_error(e, self.read_timeout))
            }
            Err(_) => {
                buf.clear();
                Err(TransportError::Timeout(self.read_timeout))
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::duplex;

    #[tokio::test]
    async fn serial_frame_transport_round_trips_terminated_frames() {
        let (mut server, client) = duplex(64);
        let mut transport = SerialFrameTransport::new(client, b'\n', 32);

        tokio::spawn(async move {
            tokio::io::AsyncWriteExt::write_all(&mut server, b"hello\n")
                .await
                .unwrap();
        });

        let mut buf = Vec::new();
        transport.recv_frame(&mut buf).await.unwrap();
        assert_eq!(buf, b"hello\n");
    }

    #[tokio::test]
    async fn serial_frame_transport_recv_frame_propagates_eof() {
        // A closed peer produces an immediate read of zero bytes.
        let reader = Cursor::new(Vec::<u8>::new());
        let writer = Vec::<u8>::new();
        let stream = tokio::io::join(reader, writer);
        let mut transport =
            SerialFrameTransport::new(stream, b'\n', 32).with_read_timeout(Duration::from_secs(1));

        let mut buf = Vec::new();
        let err = transport.recv_frame(&mut buf).await.unwrap_err();
        assert!(matches!(err, TransportError::Eof), "got {err:?}");
    }

    #[tokio::test]
    async fn serial_frame_transport_recv_frame_rejects_oversized_frame() {
        // Stream emits 16 bytes with no terminator, max_frame_size is 8.
        // After max_frame_size + buffer fills, read_until returns
        // success at EOF without seeing the terminator. We require a
        // terminator to consider the frame complete, so this surfaces
        // as Framing.
        let reader = Cursor::new(b"0123456789ABCDEF".to_vec());
        let writer = Vec::<u8>::new();
        let stream = tokio::io::join(reader, writer);
        let mut transport = SerialFrameTransport::new(stream, b'\n', 8);

        let mut buf = Vec::new();
        let err = transport.recv_frame(&mut buf).await.unwrap_err();
        assert!(matches!(err, TransportError::Framing(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn serial_frame_transport_recv_frame_bounds_buf_under_runaway_writer() {
        // A peer that streams indefinitely without a terminator must
        // not be able to push `buf` past `max_frame_size`. Without the
        // incremental bound this would grow `buf` to ~1 KiB before the
        // post-check fired; with the bound, the error surfaces as soon
        // as the would-be frame crosses the cap and `buf` never
        // exceeds `max_frame_size`.
        const CAP: usize = 16;
        let (mut server, client) = duplex(1024);
        let mut transport =
            SerialFrameTransport::new(client, b'\n', CAP).with_read_timeout(Duration::from_secs(1));

        tokio::spawn(async move {
            let _ = tokio::io::AsyncWriteExt::write_all(&mut server, &[b'A'; 1024]).await;
        });

        let mut buf = Vec::new();
        let err = transport.recv_frame(&mut buf).await.unwrap_err();
        assert!(matches!(err, TransportError::Framing(_)), "got {err:?}");
        assert!(
            buf.len() <= CAP,
            "buf grew to {} bytes, exceeds max_frame_size {}",
            buf.len(),
            CAP
        );
    }

    #[tokio::test]
    async fn serial_frame_transport_send_frame_writes_bytes_verbatim() {
        // Caller-supplied terminator stays in the frame; the transport
        // does not strip or add anything.
        let (mut server, client) = duplex(64);
        let mut transport = SerialFrameTransport::new(client, b'\n', 32);

        transport.send_frame(b"ping\n").await.unwrap();

        let mut out = [0u8; 5];
        tokio::io::AsyncReadExt::read_exact(&mut server, &mut out)
            .await
            .unwrap();
        assert_eq!(&out, b"ping\n");
    }

    // ============================================================================
    // send_frame must never flush the wrapped stream: on Unix,
    // `tokio_serial::SerialStream::poll_flush` issues a blocking `tcdrain(2)`
    // ioctl that a `tokio::time::timeout` cannot preempt once entered, so a
    // peer that enumerates but never services its endpoints (dead firmware
    // behind a bus-powered USB-CDC port) wedges the write forever instead of
    // surfacing an honest timeout. See issue #622.
    // ============================================================================

    /// Test-only AsyncRead/AsyncWrite whose `poll_flush` panics — standing
    /// in for a stream where flushing would hang or block indefinitely
    /// (e.g. a `tcdrain`-class ioctl against a dead peer). `poll_write`
    /// always succeeds.
    #[derive(Default)]
    struct FlushPanicsStream;

    impl tokio::io::AsyncRead for FlushPanicsStream {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    impl tokio::io::AsyncWrite for FlushPanicsStream {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            panic!("send_frame must not flush the wrapped stream");
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn serial_frame_transport_send_frame_never_flushes_the_underlying_stream() {
        let mut transport = SerialFrameTransport::new(FlushPanicsStream, b'\n', 32);
        transport.send_frame(b"ping\n").await.unwrap();
    }

    #[tokio::test]
    async fn udp_frame_transport_round_trips_against_local_echo() {
        // Bind two sockets that send to each other. The "server"
        // echoes; the "client" runs through UdpFrameTransport.
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client.local_addr().unwrap();
        client.connect(server_addr).await.unwrap();

        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            let (n, peer) = server.recv_from(&mut buf).await.unwrap();
            assert_eq!(peer, client_addr);
            server.send_to(&buf[..n], peer).await.unwrap();
        });

        let mut transport = UdpFrameTransport::new(client, 64);
        transport.send_frame(b"hello").await.unwrap();
        let mut reply = Vec::new();
        transport.recv_frame(&mut reply).await.unwrap();
        assert_eq!(reply, b"hello");
    }

    #[tokio::test]
    async fn udp_frame_transport_rejects_oversized_send() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(server.local_addr().unwrap()).await.unwrap();

        let mut transport = UdpFrameTransport::new(client, 8);
        let err = transport
            .send_frame(b"way-too-long-payload")
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::Framing(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn udp_frame_transport_recv_times_out_when_silent() {
        let _server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = _server.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(server_addr).await.unwrap();

        let mut transport =
            UdpFrameTransport::new(client, 64).with_read_timeout(Duration::from_millis(50));
        let mut buf = Vec::new();
        let err = transport.recv_frame(&mut buf).await.unwrap_err();
        assert!(matches!(err, TransportError::Timeout(_)), "got {err:?}");
    }

    // ============================================================================
    // classify_io_error: TimedOut from a wrapped stream surfaces as Timeout,
    // not Io. This is the structural guarantee that lets services build
    // factories without worrying about which timeout layer fires first
    // (port-level VTIME, async runtime deadline, OS recv timer, …) — every
    // io::Error with kind TimedOut from the wrapped stream gets reclassified
    // here. See PR #280 for the bug class this prevents.
    // ============================================================================

    /// Test-only AsyncRead/AsyncWrite that returns
    /// `io::ErrorKind::TimedOut` on the next read or write, simulating
    /// what a port-level (termios `VTIME`) timeout looks like through
    /// tokio-serial's `AsyncRead` impl.
    struct TimedOutStream {
        fail_next_read: bool,
        fail_next_write: bool,
    }

    impl tokio::io::AsyncRead for TimedOutStream {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            if self.fail_next_read {
                self.fail_next_read = false;
                std::task::Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "simulated port-level read timeout",
                )))
            } else {
                std::task::Poll::Ready(Ok(()))
            }
        }
    }

    impl tokio::io::AsyncWrite for TimedOutStream {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            if self.fail_next_write {
                self.fail_next_write = false;
                std::task::Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "simulated port-level write timeout",
                )))
            } else {
                std::task::Poll::Ready(Ok(buf.len()))
            }
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn classify_io_error_maps_timed_out_to_timeout_variant() {
        let e = io::Error::new(io::ErrorKind::TimedOut, "x");
        let t = classify_io_error(e, Duration::from_secs(2));
        assert!(matches!(t, TransportError::Timeout(d) if d == Duration::from_secs(2)));
    }

    #[tokio::test]
    async fn classify_io_error_passes_through_non_timeout_kinds() {
        let e = io::Error::new(io::ErrorKind::BrokenPipe, "broken");
        let t = classify_io_error(e, Duration::from_secs(2));
        match t {
            TransportError::Io(inner) => assert_eq!(inner.kind(), io::ErrorKind::BrokenPipe),
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn serial_frame_transport_read_timed_out_io_surfaces_as_timeout() {
        // Wrap a stream whose underlying read returns ErrorKind::TimedOut.
        // Without the classification, this would surface as
        // TransportError::Io(TimedOut) — and the legacy qhy-focuser /
        // ppba-driver factories that set `.timeout(...)` on the
        // tokio-serial builder relied on accidental termios behaviour
        // for the kind, with no guarantee the variant ever lined up.
        let stream = TimedOutStream {
            fail_next_read: true,
            fail_next_write: false,
        };
        let mut transport =
            SerialFrameTransport::new(stream, b'\n', 32).with_read_timeout(Duration::from_secs(3));
        let mut buf = Vec::new();
        let err = transport.recv_frame(&mut buf).await.unwrap_err();
        match err {
            TransportError::Timeout(d) => assert_eq!(d, Duration::from_secs(3)),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn serial_frame_transport_write_timed_out_io_surfaces_as_timeout() {
        let stream = TimedOutStream {
            fail_next_read: false,
            fail_next_write: true,
        };
        let mut transport =
            SerialFrameTransport::new(stream, b'\n', 32).with_write_timeout(Duration::from_secs(4));
        let err = transport.send_frame(b"ping\n").await.unwrap_err();
        match err {
            TransportError::Timeout(d) => assert_eq!(d, Duration::from_secs(4)),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }
}

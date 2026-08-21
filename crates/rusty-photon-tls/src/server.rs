use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
// tokio's Instant, not std's: identical to std::time::Instant in production,
// but also tracks tokio's paused/virtual clock under `#[tokio::test(start_paused
// = true)]` — std::time::Instant would silently ignore tokio::time::advance(),
// making the shared-deadline logic below untestable under paused time.
use tokio::time::{timeout, timeout_at, Instant};
use tokio_rustls::TlsAcceptor;
use tracing::debug;

use crate::config::TlsConfig;
use crate::error::Result;
use crate::resolver::ReloadableCertResolver;

/// First byte of a TLS handshake record (`ContentType::Handshake`,
/// RFC 8446 §5.1) — distinguishes a real TLS client from a plaintext HTTP
/// request landing on the TLS port (issue #610).
const TLS_HANDSHAKE_CONTENT_TYPE: u8 = 0x16;

/// Bound on how long a non-TLS connection may take, in total, to produce a
/// recognizable HTTP request head before it is dropped. A single deadline
/// computed once (in `handle_connection`) covers both the initial byte peek
/// and reading the request head, so a connection that never sends anything —
/// or trickles bytes forever — cannot become a resource sink on the TLS port
/// for longer than this, not this budget twice over.
const PLAINTEXT_IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound on the buffered size of a plaintext request head (request line +
/// headers) while looking for the blank line that ends them.
const MAX_REQUEST_HEAD_BYTES: usize = 8 * 1024;

/// Bound on how long a TLS handshake may take once a `0x16` first byte is
/// seen. A client (or attacker) that starts a handshake and then stalls
/// would otherwise hold `acceptor.accept()` — and its spawned task — open
/// indefinitely, defeating the same resource-sink guarantee the plaintext
/// path already gets. Longer than `PLAINTEXT_IO_TIMEOUT` because a genuine
/// handshake, unlike the plaintext redirect's single request/response, can
/// span multiple round trips on a slow or high-latency link (ADR-002's
/// remote-observatory-over-VPN case).
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Bind a TCP listener with dual-stack (IPv4+IPv6) support.
///
/// This replicates the socket2 binding logic from `ascom-alpaca-rs`'s
/// `Server::bind()`, ensuring consistent behavior on all platforms.
/// On Linux, dual-stack is the default for IPv6 sockets, but on Windows
/// it is not — this function explicitly sets `only_v6(false)`.
///
/// # Errors
///
/// Returns the I/O error if creating, configuring, binding, or
/// listening on the socket fails (e.g. the port is already taken).
pub fn bind_dual_stack(addr: SocketAddr) -> Result<std::net::TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;

    if addr.is_ipv6() {
        socket.set_only_v6(false)?;
    }

    // On Unix, set SO_REUSEADDR so the server can rebind its port immediately
    // across a restart or in-process reload, even while a previous listener's
    // accepted connections (an HTTP client's keep-alive pool) or TIME_WAIT
    // sockets still linger — without it a reloading service (dsd-fp2's
    // `config.apply`) fails to rebind with `AddrInUse`. On Unix this still
    // rejects a second *live* listener on the port, so it can't mask an
    // "already running" error.
    //
    // We deliberately do NOT set SO_REUSEADDR on Windows, where its semantics
    // differ — there it would let another process bind (hijack) the same port.
    // Windows already lets the original owner rebind, so the reload works there
    // without it. (The exclusive-bind opt-in, SO_EXCLUSIVEADDRUSE, isn't exposed
    // by socket2; the default Windows behaviour is the safe choice here.)
    #[cfg(unix)]
    socket.set_reuse_address(true)?;

    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(128)?;

    Ok(socket.into())
}

/// Bind a TCP listener with dual-stack support and return a tokio `TcpListener`.
///
/// # Errors
///
/// Returns the I/O error if the bind fails or the listener cannot be
/// registered with the tokio reactor.
#[expect(
    clippy::unused_async,
    reason = "`TcpListener::from_std` panics outside a tokio reactor; the async signature forces callers into one"
)]
pub async fn bind_dual_stack_tokio(addr: SocketAddr) -> Result<TcpListener> {
    let std_listener = bind_dual_stack(addr)?;
    let listener = TcpListener::from_std(std_listener)?;
    Ok(listener)
}

/// Load TLS certificate and key from a `TlsConfig`, returning a `TlsAcceptor`
/// that hot-reloads the pair when the files change on disk (see
/// [`ReloadableCertResolver`]).
///
/// # Errors
///
/// Returns a [`crate::error::TlsError`] if the certificate or key
/// cannot be read or parsed.
pub fn build_tls_acceptor(tls_config: &TlsConfig) -> Result<TlsAcceptor> {
    let cert_path = tls_config.resolved_cert_path();
    let key_path = tls_config.resolved_key_path();

    debug!("Loading TLS cert from {}", cert_path.display());
    debug!("Loading TLS key from {}", key_path.display());

    let resolver = ReloadableCertResolver::load(cert_path, key_path)?;
    Ok(acceptor_from_resolver(resolver))
}

/// Wrap a [`ReloadableCertResolver`] into a `TlsAcceptor` — the seam tests
/// use to shorten the resolver's check interval before serving.
pub fn acceptor_from_resolver(resolver: ReloadableCertResolver) -> TlsAcceptor {
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
    TlsAcceptor::from(Arc::new(server_config))
}

/// Serve an axum router over TLS on the given listener.
///
/// The `shutdown` future is polled to initiate graceful shutdown.
///
/// # Errors
///
/// Returns a [`crate::error::TlsError`] if the certificate/key pair
/// cannot be loaded, or the I/O error if the accept loop fails.
pub async fn serve_tls<F>(
    listener: TcpListener,
    router: axum::Router,
    tls_config: &TlsConfig,
    shutdown: F,
) -> Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let acceptor = build_tls_acceptor(tls_config)?;

    debug!(
        "Starting TLS server on {}",
        listener
            .local_addr()
            .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)))
    );

    serve_tls_with_acceptor(listener, router, acceptor, shutdown).await
}

/// Serve an axum router over plain HTTP on the given listener.
///
/// The `shutdown` future is polled to initiate graceful shutdown.
///
/// # Errors
///
/// Returns the I/O error if the axum serve loop fails.
pub async fn serve_plain<F>(listener: TcpListener, router: axum::Router, shutdown: F) -> Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    debug!(
        "Starting plain HTTP server on {}",
        listener
            .local_addr()
            .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)))
    );

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await?;

    Ok(())
}

/// Run an axum TLS server loop using tokio-rustls with a caller-built
/// acceptor (e.g. one from [`acceptor_from_resolver`]).
///
/// Accepts TCP connections, wraps them with TLS, and serves the router.
/// Stops accepting new connections when `shutdown` completes.
///
/// # Errors
///
/// Returns the I/O error if accepting connections fails; per-connection
/// TLS handshake failures are logged and skipped, not returned.
pub async fn serve_tls_with_acceptor<F>(
    listener: TcpListener,
    router: axum::Router,
    acceptor: TlsAcceptor,
    shutdown: F,
) -> Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    use tokio::pin;

    let shutdown = shutdown;
    pin!(shutdown);

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, remote_addr) = result?;
                let acceptor = acceptor.clone();
                let router = router.clone();

                tokio::spawn(async move {
                    handle_connection(stream, remote_addr, acceptor, router).await;
                });
            }
            () = &mut shutdown => {
                debug!("TLS server shutting down");
                break;
            }
        }
    }

    Ok(())
}

/// Dispatch one accepted connection on the TLS port: a genuine TLS
/// handshake (first byte `0x16`) proceeds as before; anything else is
/// handled as a possibly-plaintext HTTP request and answered with a
/// redirect to `https://` on the same host and port, or dropped if it
/// doesn't look like HTTP at all (issue #610).
async fn handle_connection(
    stream: TcpStream,
    remote_addr: SocketAddr,
    acceptor: TlsAcceptor,
    router: axum::Router,
) {
    // One deadline shared across the initial byte peek and (if this turns
    // out to be plaintext) the request-head read, so the total resource-sink
    // bound is PLAINTEXT_IO_TIMEOUT exactly — not that budget twice over
    // from two independently-started timeouts.
    // `checked_add` cannot fail for a constant a few seconds long; if the
    // clock ever were that close to its representable end, degrading to an
    // already-expired deadline drops the connection — the fail-closed shape
    // for a resource-sink guard.
    let now = Instant::now();
    let deadline = now.checked_add(PLAINTEXT_IO_TIMEOUT).unwrap_or(now);

    let mut first_byte = [0u8; 1];
    let peeked = match timeout_at(deadline, stream.peek(&mut first_byte)).await {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => {
            debug!("Failed to peek connection from {}: {}", remote_addr, e);
            return;
        }
        Err(_) => {
            debug!(
                "Timed out waiting for the first byte from {}; dropping",
                remote_addr
            );
            return;
        }
    };
    if peeked == 0 {
        // Peer closed the connection before sending anything.
        return;
    }

    if first_byte[0] == TLS_HANDSHAKE_CONTENT_TYPE {
        let tls_stream = match timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                debug!("TLS handshake failed from {}: {}", remote_addr, e);
                return;
            }
            Err(_) => {
                debug!(
                    "TLS handshake from {} did not complete within {:?}; dropping",
                    remote_addr, TLS_HANDSHAKE_TIMEOUT
                );
                return;
            }
        };

        let io = TokioIo::new(tls_stream);
        let service = TowerToHyperService::new(router.into_service());

        if let Err(e) =
            hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection(io, service)
                .await
        {
            debug!("Error serving TLS connection from {}: {}", remote_addr, e);
        }
        return;
    }

    debug!(
        "Non-TLS byte from {} on the TLS port; treating as plaintext HTTP",
        remote_addr
    );
    redirect_plaintext_http(stream, remote_addr, deadline).await;
}

/// Answer a connection that didn't start with a TLS handshake byte: parse a
/// bounded plaintext HTTP request head and reply with a redirect to the same
/// host and port under `https://`. Anything that doesn't resolve to a
/// parseable HTTP request within `deadline` (the same deadline the caller
/// already started for the byte peek) is dropped without a response — only
/// bytes that look like HTTP earn a reply on the TLS port.
async fn redirect_plaintext_http(
    mut stream: TcpStream,
    remote_addr: SocketAddr,
    deadline: Instant,
) {
    let local_addr = match stream.local_addr() {
        Ok(addr) => addr,
        Err(e) => {
            debug!(
                "Failed to read the local address for {}: {}",
                remote_addr, e
            );
            return;
        }
    };

    let Some(request) = read_request_head(&mut stream, deadline).await else {
        debug!(
            "Dropping non-HTTP plaintext connection from {}",
            remote_addr
        );
        return;
    };

    let response = build_redirect_response(&request, local_addr);
    match timeout_at(deadline, stream.write_all(response.as_bytes())).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            debug!(
                "Failed to write the plaintext redirect to {}: {}",
                remote_addr, e
            );
            return;
        }
        Err(_) => {
            debug!(
                "Timed out writing the plaintext redirect to {}",
                remote_addr
            );
            return;
        }
    }
    let _ = timeout_at(deadline, stream.shutdown()).await;
}

/// A minimally-parsed plaintext HTTP request head.
struct ParsedRequest {
    /// The raw request-target from the request line (path + optional query).
    target: String,
    /// The `Host` header value, if present, port suffix and all.
    host: Option<String>,
}

/// Read a plaintext request head (request line + headers) up to the blank
/// line that ends them, bounded by [`MAX_REQUEST_HEAD_BYTES`] and by
/// `deadline` — shared with the caller's initial byte peek, so the total
/// time this connection may occupy a task is [`PLAINTEXT_IO_TIMEOUT`], not
/// that budget twice over. Returns `None` if the connection closes, times
/// out, exceeds the size bound, or the bytes read so far don't look like an
/// HTTP request line — every `None` case means the caller drops the
/// connection without a response.
async fn read_request_head(stream: &mut TcpStream, deadline: Instant) -> Option<ParsedRequest> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];

    loop {
        if let Some(line_end) = find_subslice(&buf, b"\r\n") {
            // Bail as soon as the request line itself doesn't look like
            // HTTP, rather than waiting out the full timeout on garbage.
            parse_request_head(buf.get(..line_end)?)?;
        }
        if let Some(headers_end) = find_subslice(&buf, b"\r\n\r\n") {
            return parse_request_head(buf.get(..headers_end)?);
        }
        let read_len = capped_read_len(buf.len(), chunk.len());
        if read_len == 0 {
            return None;
        }

        match timeout_at(deadline, stream.read(chunk.get_mut(..read_len)?)).await {
            Ok(Ok(0) | Err(_)) | Err(_) => return None,
            Ok(Ok(n)) => buf.extend_from_slice(chunk.get(..n)?),
        }
    }
}

/// How many more bytes [`read_request_head`] may read into its 512-byte
/// chunk buffer without letting the accumulated head exceed
/// [`MAX_REQUEST_HEAD_BYTES`] — a single `read()` filling the whole chunk
/// could otherwise overshoot the advertised cap by up to `chunk_len` bytes.
fn capped_read_len(buf_len: usize, chunk_len: usize) -> usize {
    MAX_REQUEST_HEAD_BYTES
        .saturating_sub(buf_len)
        .min(chunk_len)
}

/// `true`-shaped result only when `head` starts with `METHOD target
/// HTTP/x.y` — the minimal shape needed before plaintext bytes on the TLS
/// port earn a reply at all. `head` may be just the request line (no
/// trailing headers yet) or a full head ending at the blank line.
fn parse_request_head(head: &[u8]) -> Option<ParsedRequest> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next()?;

    let mut parts = request_line.splitn(3, ' ');
    let method = parts.next()?;
    let target = parts.next()?;
    let version = parts.next()?;

    // RFC 7230 §3.1.1: method is a `token` (1*tchar), not restricted to
    // uppercase letters — extension methods like WebDAV's PROPFIND/MKCOL or
    // SSDP's M-SEARCH are syntactically valid and would otherwise be
    // silently dropped instead of redirected. The method is never echoed
    // into the response, so broadening this doesn't reopen any injection
    // surface; a bare '\r'/'\n' still isn't a tchar and stays rejected.
    if method.is_empty() || !method.bytes().all(is_tchar) {
        return None;
    }
    // HTTP/1.x only: the HTTP/2 cleartext preface ("PRI * HTTP/2.0\r\n\r\n...")
    // is a fixed 24-byte magic string that otherwise parses as a well-formed
    // request line (method `PRI`, target `*`) — accepting `HTTP/2.0` here
    // would redirect that non-HTTP-1.x prelude instead of dropping it.
    if !version.starts_with("HTTP/1.") {
        return None;
    }
    // Restrict to origin-form targets (`/path`) and the OPTIONS asterisk-form
    // (`*`, valid only for OPTIONS per RFC 7230 §5.3.4) — the only shapes a
    // redirect can append after `https://host:port` and still mean the same
    // thing. Absolute-form (`http://host/path`) and authority-form (`CONNECT
    // host:port`) targets are proxy-only requests that don't belong on this
    // port; redirecting them would either be malformed or, worse, let a
    // client-supplied target steer the `Location` header's authority. A
    // target is also rejected if it contains a bare `\r`/`\n`: splitting on
    // the literal "\r\n" sequence doesn't stop a lone CR or LF from hiding
    // inside what otherwise looks like a valid `/path` token, and
    // interpolating one into the `Location` header verbatim would let a
    // crafted request inject its own header line into our response (CWE-113).
    match target {
        "*" if method == "OPTIONS" => {}
        "*" => return None,
        t if t.starts_with('/') && !t.contains(['\r', '\n']) => {}
        _ => return None,
    }

    let mut host = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("host") {
                let value = value.trim();
                // An empty/whitespace-only Host value (e.g. "Host: \r\n")
                // must fall back to the local IP, not build a Location with
                // a missing authority (`https://:<port>...`). Same for a
                // value smuggling a bare `\r`/`\n` — that's the same
                // header-injection risk as the target check above, and
                // "invalid" here means "don't echo it", not "reject the
                // whole request": the client still gets a redirect, just to
                // the local IP instead of its claimed Host.
                if !value.is_empty() && !value.contains(['\r', '\n']) {
                    host = Some(value.to_string());
                }
            }
        }
    }

    Some(ParsedRequest {
        target: target.to_string(),
        host,
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// RFC 7230 §3.2.6 `tchar` — the character class allowed in an HTTP method
/// token (and header field-names, though this crate only uses it for the
/// method): any alphanumeric ASCII byte, plus a fixed set of punctuation.
/// Notably excludes whitespace and control characters (including `\r`/`\n`).
const fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// A hostname/IPv4 authority is built only from letters, digits, hyphens
/// and dots (`_` is technically illegal DNS but common enough in the wild
/// to allow). Anything else — `@`, `/`, `?`, `#`, whitespace, backslash —
/// is a URI delimiter or userinfo separator that a downstream URL parser
/// could read differently than intended if reflected verbatim into the
/// `Location` authority (e.g. `Host: trusted.local@evil.example` parsed
/// with userinfo `trusted.local` and actual host `evil.example`).
const fn is_host_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_')
}

/// A bracketed IPv6 literal is built only from hex digits, `:` (and `.`
/// for an IPv4-mapped form like `::ffff:192.0.2.1`).
const fn is_ipv6_literal_byte(b: u8) -> bool {
    b.is_ascii_hexdigit() || matches!(b, b':' | b'.')
}

/// Strip a trailing `:port` from a `Host` header value, respecting
/// bracketed IPv6 literals (`[::1]:8443`). Returns `None` when the result
/// can't be a syntactically valid `Location` authority, in which case the
/// caller falls back to its own local IP instead of producing a malformed
/// or misleading redirect: an unbracketed IPv6 literal (`Host: ::1`,
/// invalid per RFC 7230 but not unheard of from a malformed client), a
/// bracketed literal with invalid trailing bytes, or a hostname/IPv4
/// containing characters outside `is_host_byte` (see its doc comment for
/// why that's a redirect-confusion risk, not just cosmetics).
fn strip_host_port(host: &str) -> Option<&str> {
    if let Some(rest) = host.strip_prefix('[') {
        let end = rest.find(']')?;
        let literal = rest.get(..end)?;
        if literal.is_empty() || !literal.bytes().all(is_ipv6_literal_byte) {
            return None;
        }
        let after = rest.get(end.saturating_add(1)..)?;
        if !after.is_empty() {
            let port = after.strip_prefix(':')?;
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
        }
        // `end + 2` re-includes the `[` the prefix strip removed and the `]`.
        return host.get(..end.saturating_add(2));
    }
    let name = match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    };
    if !name.is_empty() && name.bytes().all(is_host_byte) {
        Some(name)
    } else {
        None
    }
}

fn format_ip_for_url(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V4(v4) => v4.to_string(),
        std::net::IpAddr::V6(v6) => format!("[{v6}]"),
    }
}

/// Build the minimal `308 Permanent Redirect` response: the `Host` header
/// (port stripped) if the client sent one, else the connection's local IP —
/// always paired with the TLS listener's own port, never whatever port (if
/// any) the client's `Host` header claimed, so the redirect always lands
/// back on this same TLS port.
fn build_redirect_response(request: &ParsedRequest, local_addr: SocketAddr) -> String {
    let host = request
        .host
        .as_deref()
        .and_then(strip_host_port)
        .map_or_else(|| format_ip_for_url(local_addr.ip()), str::to_string);
    // "*" (OPTIONS's server-wide form) is a request-line-only construct —
    // it isn't valid URI-path syntax, so appended as-is it would produce a
    // Location like "https://host:port*" with no path separator. Map it to
    // the root path instead of rejecting the request outright, so an
    // OPTIONS * client still gets redirected rather than silently dropped.
    let path = if request.target == "*" {
        "/"
    } else {
        request.target.as_str()
    };
    let location = format!("https://{host}:{}{}", local_addr.port(), path);
    format!(
        "HTTP/1.1 308 Permanent Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::error::TlsError;

    #[test]
    fn bind_dual_stack_on_port_zero() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = bind_dual_stack(addr).unwrap();
        let bound_addr = listener.local_addr().unwrap();
        assert_ne!(bound_addr.port(), 0, "should be assigned a real port");
    }

    #[test]
    fn bind_dual_stack_rebinds_port_with_lingering_connection() {
        // A service that reloads tears down its listener and rebinds the same
        // port while a client's previous connection may still linger on it.
        // SO_REUSEADDR must let the rebind succeed instead of failing
        // `AddrInUse`. This guards the OS-sensitive rebind behaviour (dsd-fp2's
        // `config.apply` reload) across platforms.
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = bind_dual_stack(addr).unwrap();
        // bind_dual_stack sets non-blocking; block in accept() for the test.
        listener.set_nonblocking(false).unwrap();
        let bound = listener.local_addr().unwrap();

        // Establish and accept a connection, then drop the listener while the
        // accepted connection lingers — independent of the listening socket.
        let client = std::net::TcpStream::connect(bound).unwrap();
        let (server_conn, _) = listener.accept().unwrap();
        drop(listener);

        // Rebinding the same port must succeed thanks to SO_REUSEADDR.
        let rebind = bind_dual_stack(bound);
        assert!(
            rebind.is_ok(),
            "rebind of {bound} failed (SO_REUSEADDR regression?): {:?}",
            rebind.err()
        );

        drop(server_conn);
        drop(client);
    }

    #[test]
    fn bind_dual_stack_ipv6() {
        let addr: SocketAddr = "[::]:0".parse().unwrap();
        // This may fail on systems without IPv6 support, so we just
        // check it doesn't panic for the wrong reason.
        match bind_dual_stack(addr) {
            Ok(listener) => {
                let bound = listener.local_addr().unwrap();
                assert_ne!(bound.port(), 0);
            }
            Err(TlsError::Io(e)) if e.kind() == std::io::ErrorKind::AddrNotAvailable => {
                // IPv6 not available on this system, that's fine
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn parse_request_head_extracts_target_and_host() {
        let head = b"GET /setup/v1/camera/0/setup HTTP/1.1\r\nHost: 127.0.0.1:11121\r\nUser-Agent: test\r\n";
        let parsed = parse_request_head(head).unwrap();
        assert_eq!(parsed.target, "/setup/v1/camera/0/setup");
        assert_eq!(parsed.host.as_deref(), Some("127.0.0.1:11121"));
    }

    #[test]
    fn parse_request_head_ignores_a_header_line_without_a_colon() {
        // A malformed header line with no ':' at all can't be split into a
        // name/value pair — it's simply skipped, and a well-formed Host
        // header later in the same request still gets picked up.
        let head =
            b"GET /health HTTP/1.1\r\nNotAHeaderLine\r\nHost: filemonitor.local\r\n".as_slice();
        let parsed = parse_request_head(head).unwrap();
        assert_eq!(parsed.host.as_deref(), Some("filemonitor.local"));
    }

    #[test]
    fn parse_request_head_without_host_header() {
        let parsed = parse_request_head(b"GET / HTTP/1.0\r\n").unwrap();
        assert_eq!(parsed.target, "/");
        assert_eq!(parsed.host, None);
    }

    #[test]
    fn parse_request_head_treats_empty_host_value_as_absent() {
        // An empty/whitespace-only Host value must fall back to the local
        // IP in build_redirect_response, not produce a Location with a
        // missing authority ("https://:<port>...").
        let parsed = parse_request_head(b"GET / HTTP/1.1\r\nHost: \r\n").unwrap();
        assert_eq!(parsed.host, None);
        let parsed = parse_request_head(b"GET / HTTP/1.1\r\nHost:    \r\n").unwrap();
        assert_eq!(parsed.host, None);
    }

    #[test]
    fn parse_request_head_rejects_a_target_smuggling_a_bare_newline() {
        // Splitting on the literal "\r\n" sequence doesn't stop a lone LF
        // from hiding inside what otherwise parses as a valid /path token;
        // letting it through would inject a header line into our response
        // via build_redirect_response's Location interpolation (CWE-113).
        assert!(parse_request_head(b"GET /path\nInjected:x HTTP/1.1\r\n").is_none());
    }

    #[test]
    fn parse_request_head_treats_a_host_value_smuggling_a_bare_newline_as_absent() {
        // Same injection risk as the target check, but for the Host header:
        // an embedded bare LF must not be echoed into the Location value.
        let parsed =
            parse_request_head(b"GET / HTTP/1.1\r\nHost: example.com\nInjected: y\r\n\r\n")
                .unwrap();
        assert_eq!(parsed.host, None);
    }

    #[test]
    fn parse_request_head_rejects_non_http_bytes() {
        assert!(parse_request_head(b"random garbage bytes, not HTTP at all").is_none());
    }

    #[test]
    fn parse_request_head_accepts_rfc7230_extension_methods() {
        // Method is a `token` (1*tchar) per RFC 7230 §3.1.1, not restricted
        // to uppercase letters — WebDAV's PROPFIND and SSDP's M-SEARCH (with
        // a hyphen, a valid tchar) are syntactically valid and must be
        // redirected, not silently dropped.
        let parsed = parse_request_head(b"PROPFIND /calendar HTTP/1.1\r\n").unwrap();
        assert_eq!(parsed.target, "/calendar");
        let parsed = parse_request_head(b"M-SEARCH /device HTTP/1.1\r\n").unwrap();
        assert_eq!(parsed.target, "/device");
    }

    #[test]
    fn parse_request_head_rejects_a_method_smuggling_a_bare_newline() {
        // A bare CR/LF is never a tchar, so this must still be rejected even
        // under the broadened method check.
        assert!(parse_request_head(b"GET\nInjectedX /path HTTP/1.1\r\n").is_none());
    }

    #[test]
    fn parse_request_head_rejects_tls_handshake_bytes() {
        // A real TLS ClientHello record never reaches this parser (the 0x16
        // sniff routes it to the acceptor instead), but the parser must
        // still reject it defensively rather than mis-parse binary noise.
        assert!(parse_request_head(&[0x16, 0x03, 0x01, 0x00, 0x05]).is_none());
    }

    #[test]
    fn parse_request_head_accepts_asterisk_target() {
        let parsed = parse_request_head(b"OPTIONS * HTTP/1.1\r\n").unwrap();
        assert_eq!(parsed.target, "*");
    }

    #[test]
    fn parse_request_head_rejects_asterisk_target_for_non_options_methods() {
        // "*" is only meaningful for OPTIONS (RFC 7230 §5.3.4); other
        // methods sending it are not a request shape we redirect.
        assert!(parse_request_head(b"GET * HTTP/1.1\r\n").is_none());
    }

    #[test]
    fn parse_request_head_rejects_http2_cleartext_preface() {
        // The HTTP/2 client connection preface starts with a fixed line
        // that otherwise parses as a well-formed HTTP/1.x request line
        // (method PRI, target *); it must be dropped, not redirected.
        assert!(parse_request_head(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n").is_none());
    }

    #[test]
    fn parse_request_head_rejects_absolute_form_target() {
        // A proxy-style absolute-form target would otherwise be appended
        // verbatim after `https://host:port`, producing a malformed (or
        // attacker-steerable) `Location` header.
        assert!(parse_request_head(b"GET http://evil.example/path HTTP/1.1\r\n").is_none());
    }

    #[test]
    fn parse_request_head_rejects_authority_form_target() {
        // CONNECT's authority-form target ("host:port", no leading slash)
        // is a proxy request shape, not a same-origin path.
        assert!(parse_request_head(b"CONNECT evil.example:443 HTTP/1.1\r\n").is_none());
    }

    #[test]
    fn capped_read_len_never_lets_the_buffer_exceed_the_cap() {
        assert_eq!(capped_read_len(0, 512), 512);
        assert_eq!(capped_read_len(MAX_REQUEST_HEAD_BYTES - 100, 512), 100);
        assert_eq!(capped_read_len(MAX_REQUEST_HEAD_BYTES, 512), 0);
        assert_eq!(capped_read_len(MAX_REQUEST_HEAD_BYTES + 1, 512), 0);
    }

    #[test]
    fn strip_host_port_handles_ipv4_and_hostnames() {
        assert_eq!(strip_host_port("127.0.0.1:11121"), Some("127.0.0.1"));
        assert_eq!(
            strip_host_port("filemonitor.local"),
            Some("filemonitor.local")
        );
        assert_eq!(
            strip_host_port("filemonitor.local:11121"),
            Some("filemonitor.local")
        );
    }

    #[test]
    fn strip_host_port_handles_bracketed_ipv6() {
        assert_eq!(strip_host_port("[::1]:11121"), Some("[::1]"));
        assert_eq!(strip_host_port("[::1]"), Some("[::1]"));
    }

    #[test]
    fn strip_host_port_rejects_unbracketed_ipv6() {
        // "Host: ::1" is invalid per RFC 7230 (IPv6 literals must be
        // bracketed), but a malformed client could still send it; without
        // this guard it would produce a garbled Location authority.
        assert_eq!(strip_host_port("::1"), None);
    }

    #[test]
    fn strip_host_port_rejects_userinfo_and_uri_delimiters() {
        // A URL parser downstream of the redirect could read
        // "trusted.local@evil.example" as userinfo "trusted.local" for
        // host "evil.example" — reflecting these bytes verbatim would be a
        // redirect-confusion risk, so fall back to the local IP instead.
        assert_eq!(strip_host_port("trusted.local@evil.example"), None);
        assert_eq!(strip_host_port("good.local/evil"), None);
        assert_eq!(strip_host_port("good.local?x=1"), None);
        assert_eq!(strip_host_port("good.local#frag"), None);
        assert_eq!(strip_host_port("good.local evil"), None);
    }

    #[test]
    fn strip_host_port_rejects_bracketed_ipv6_with_invalid_trailing_bytes() {
        assert_eq!(strip_host_port("[::1]evil.example"), None);
        assert_eq!(strip_host_port("[::1]:notaport"), None);
        assert_eq!(strip_host_port("[not-hex]"), None);
        assert_eq!(strip_host_port("[]"), None);
    }

    #[test]
    fn build_redirect_response_falls_back_to_local_ip_for_unbracketed_ipv6_host() {
        let request = ParsedRequest {
            target: "/health".to_string(),
            host: Some("::1".to_string()),
        };
        let local_addr: SocketAddr = "127.0.0.1:11121".parse().unwrap();
        let response = build_redirect_response(&request, local_addr);
        assert!(
            response.contains("Location: https://127.0.0.1:11121/health\r\n"),
            "{response}"
        );
    }

    #[test]
    fn build_redirect_response_falls_back_to_local_ip_for_a_host_smuggling_userinfo() {
        let request = ParsedRequest {
            target: "/health".to_string(),
            host: Some("trusted.local@evil.example".to_string()),
        };
        let local_addr: SocketAddr = "127.0.0.1:11121".parse().unwrap();
        let response = build_redirect_response(&request, local_addr);
        assert!(
            response.contains("Location: https://127.0.0.1:11121/health\r\n"),
            "must not reflect the userinfo-smuggling Host into the Location authority: {response}"
        );
    }

    #[test]
    fn build_redirect_response_uses_host_header_and_local_port() {
        let request = ParsedRequest {
            target: "/health".to_string(),
            host: Some("127.0.0.1:9999".to_string()),
        };
        let local_addr: SocketAddr = "127.0.0.1:11121".parse().unwrap();
        let response = build_redirect_response(&request, local_addr);
        assert!(
            response.starts_with("HTTP/1.1 308 Permanent Redirect\r\n"),
            "{response}"
        );
        assert!(
            response.contains("Location: https://127.0.0.1:11121/health\r\n"),
            "the redirect must use the TLS listener's own port, not the client's Host header port: {response}"
        );
        assert!(response.contains("Content-Length: 0\r\n"), "{response}");
        assert!(response.contains("Connection: close\r\n"), "{response}");
    }

    #[test]
    fn build_redirect_response_falls_back_to_local_ip_without_host_header() {
        let request = ParsedRequest {
            target: "/health?ClientID=1".to_string(),
            host: None,
        };
        let local_addr: SocketAddr = "192.168.1.5:11121".parse().unwrap();
        let response = build_redirect_response(&request, local_addr);
        assert!(response.contains("Location: https://192.168.1.5:11121/health?ClientID=1\r\n"));
    }

    #[test]
    fn build_redirect_response_brackets_an_ipv6_local_addr_fallback() {
        let request = ParsedRequest {
            target: "/health".to_string(),
            host: None,
        };
        let local_addr: SocketAddr = "[::1]:11121".parse().unwrap();
        let response = build_redirect_response(&request, local_addr);
        assert!(
            response.contains("Location: https://[::1]:11121/health\r\n"),
            "{response}"
        );
    }

    #[test]
    fn build_redirect_response_maps_asterisk_target_to_root_path() {
        // "*" isn't valid URI-path syntax; appended verbatim it would
        // produce a Location with no path separator ("https://host:port*").
        let request = ParsedRequest {
            target: "*".to_string(),
            host: Some("127.0.0.1:9999".to_string()),
        };
        let local_addr: SocketAddr = "127.0.0.1:11121".parse().unwrap();
        let response = build_redirect_response(&request, local_addr);
        assert!(
            response.contains("Location: https://127.0.0.1:11121/\r\n"),
            "{response}"
        );
    }
}

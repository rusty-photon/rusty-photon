//! SSE client for `rp`'s `/api/events/subscribe` stream (design § Event
//! Subscription): connects when a session starts, tails the stream,
//! reconnects with `Last-Event-ID`, and forwards each envelope's
//! event-type name + `payload` to the engine's [`EventIntake`].
//!
//! Loss handling follows the design's stance that events can always be
//! missed across an outage: a reconnect replays exactly within `rp`'s
//! 512-envelope retention; a `stream_gap` (cursor evicted, or this
//! consumer lagged) is logged at `info!` and the stream simply continues.
//! The client task ends when the engine drops the intake — a session's
//! subscription lives exactly as long as its run.
//!
//! The transport is the workspace house pattern for SSE consumption
//! (`sentinel`'s watchdog, `bdd-infra`'s harness client): a long-lived
//! `GET` read chunk by chunk with [`reqwest::Response::chunk`] — no
//! `stream` cargo feature — and a hand-rolled parser for the
//! `id:`/`event:`/`data:` line protocol.

use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::RpConnection;

use crate::engine::{EngineEvent, EventIntake};

/// Pause between reconnect attempts after a dropped stream or a failed
/// connect. Short enough that a restarting `rp` is picked up well inside
/// its 512-envelope retention; long enough not to hammer a dead endpoint.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);

/// Intake buffer depth, matching `rp`'s own broadcast buffer. When the
/// engine is mid-instruction the buffer absorbs the stream; if it fills,
/// backpressure propagates to the socket and `rp` eventually cuts this
/// consumer loose with a `stream_gap` — the designed slow-consumer path.
const EVENT_BUFFER: usize = 256;

/// Cap on one whole subscribe attempt — TCP connect, TLS, and the
/// response headers. `subscribe`'s initial attempt runs inline on the
/// `/invoke` path, so an endpoint that is black-holed *or* accepts the
/// connection and then never answers (reqwest's `connect_timeout` covers
/// only the TCP phase) must not hang the invocation. Reads of the
/// established stream are uncapped — an idle stream is healthy (`rp`
/// keep-alives every 15 s).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Subscribe to the SSE stream at `events_url` and return the engine's
/// intake. The **initial connect happens inline** — when this returns,
/// the subscription is live (or its first attempt has failed), so an
/// event emitted while the session's first instruction runs is already
/// being captured (design § Event Subscription). Reconnects happen on a
/// background task that exits when the returned intake is dropped; a
/// failed first attempt is retried there too — a dead stream never
/// blocks the session.
pub async fn subscribe(events_url: String, connection: &RpConnection) -> EventIntake {
    let (tx, rx) = mpsc::channel(EVENT_BUFFER);
    let (client, auth) = event_client(connection);
    let auth_header = rp_mcp_client::basic_authorization(&events_url, auth, connection.ca_path())
        .unwrap_or_else(|e| {
            warn!(error = %e, "cannot build the events Authorization header; subscribing unauthenticated");
            None
        });
    let first = connect(&client, &events_url, None, auth_header.as_ref()).await;
    tokio::spawn(client_loop(client, events_url, tx, first, auth_header));
    EventIntake::new(rx)
}

/// The subscription's HTTP client and the credential it may present —
/// CA trust + credentials per ADR-017, the same policy the MCP legs
/// apply. A broken CA file must not wedge the invoke path, so that
/// failure degrades to a default-trust client — but then the credential
/// is **withheld**: the credential is only ever paired with the client
/// that actually carries its CA pin, so `service_auth` can never ride a
/// connection that lost it. The routine reconnect loop surfaces the
/// consequence loudly.
fn event_client(
    connection: &RpConnection,
) -> (reqwest::Client, Option<&rp_mcp_client::ClientAuthConfig>) {
    match rusty_photon_tls::client::build_reqwest_client(connection.ca_path()) {
        Ok(client) => (client, connection.auth()),
        Err(e) => {
            warn!(
                error = %e,
                "cannot build the CA-trusting event client; using default trust and \
                 subscribing UNAUTHENTICATED (the credential is withheld without its CA pin)"
            );
            (reqwest::Client::new(), None)
        }
    }
}

/// One subscribe attempt, capped at [`CONNECT_TIMEOUT`] end to end;
/// `None` on a refused, rejected, timed-out, or failed connect (logged
/// at `debug!` — reconnection is routine).
async fn connect(
    client: &reqwest::Client,
    url: &str,
    last_seq: Option<u64>,
    auth: Option<&reqwest::header::HeaderValue>,
) -> Option<reqwest::Response> {
    let mut request = client.get(url).header("accept", "text/event-stream");
    if let Some(header) = auth {
        request = request.header(reqwest::header::AUTHORIZATION, header.clone());
    }
    if let Some(id) = last_seq {
        request = request.header("last-event-id", id.to_string());
    }
    let response = if let Ok(response) = tokio::time::timeout(CONNECT_TIMEOUT, request.send()).await
    {
        response
    } else {
        debug!(%url, "event stream subscribe attempt timed out");
        return None;
    };
    match response {
        Ok(response) if response.status().is_success() => {
            debug!(%url, cursor = ?last_seq, "event stream connected");
            Some(response)
        }
        Ok(response) => {
            debug!(%url, status = %response.status(), "event stream subscribe rejected");
            None
        }
        Err(e) => {
            debug!(%url, error = %e, "event stream connect failed");
            None
        }
    }
}

async fn client_loop(
    client: reqwest::Client,
    url: String,
    tx: mpsc::Sender<EngineEvent>,
    first: Option<reqwest::Response>,
    auth: Option<reqwest::header::HeaderValue>,
) {
    let mut last_seq: Option<u64> = None;
    let mut connection = first;
    loop {
        if let Some(response) = connection.take() {
            read_stream(response, &tx, &mut last_seq).await;
        }
        tokio::select! {
            () = tx.closed() => return,
            () = tokio::time::sleep(RECONNECT_BACKOFF) => {}
        }
        connection = tokio::select! {
            () = tx.closed() => return,
            connected = connect(&client, &url, last_seq, auth.as_ref()) => connected,
        };
    }
}

/// Tail one connection: parse frames, track the replay cursor, forward
/// envelopes. Returns when the stream ends (reconnect) or the intake is
/// dropped (checked by the caller via `tx.closed()`).
async fn read_stream(
    mut response: reqwest::Response,
    tx: &mpsc::Sender<EngineEvent>,
    last_seq: &mut Option<u64>,
) {
    let mut buffer = String::new();
    loop {
        let chunk = tokio::select! {
            // Intake dropped (the run ended): stop immediately and drop the
            // response so the HTTP connection closes — otherwise this task
            // could block on `chunk()` forever on a quiet stream, leaking
            // the task + connection.
            () = tx.closed() => return,
            chunk = response.chunk() => chunk,
        };
        match chunk {
            Ok(Some(chunk)) => {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                for frame in drain_frames(&mut buffer) {
                    if let Some(id) = frame.id {
                        *last_seq = Some(id);
                    }
                    let Some(event) = engine_event(frame) else {
                        continue;
                    };
                    if tx.send(event).await.is_err() {
                        return;
                    }
                }
            }
            // Stream ended or transport error: back to the caller for a
            // reconnect with the cursor.
            Ok(None) => {
                debug!("event stream ended");
                return;
            }
            Err(e) => {
                debug!(error = %e, "event stream read failed");
                return;
            }
        }
    }
}

/// One parsed SSE frame.
struct SseFrame {
    /// The `id:` line — the envelope's `event_seq`, the replay cursor.
    id: Option<u64>,
    /// The `event:` line — the envelope's event-type name.
    event: Option<String>,
    data: String,
}

/// An engine event from a frame, or `None` for the frames the engine
/// never sees: stream-control frames (`stream_gap` logged at `info!` per
/// the design, `stream_error` at `debug!`) and anything malformed.
fn engine_event(frame: SseFrame) -> Option<EngineEvent> {
    match frame.event.as_deref() {
        Some("stream_gap") => {
            // Evicted cursor or lagged consumer: events were missed. The
            // engine just continues — poll triggers re-observe current
            // state on their next cycle, and the re-entrancy contract
            // already assumes events can be missed across an outage.
            info!(detail = %frame.data, "event stream gap: some events were missed");
            None
        }
        Some("stream_error") => {
            debug!(detail = %frame.data, "event stream control frame reported an error");
            None
        }
        Some(event) => {
            let Ok(envelope) = serde_json::from_str::<Value>(&frame.data) else {
                debug!(event = %event, "discarding event frame with non-JSON data");
                return None;
            };
            let payload = envelope.get("payload").cloned().unwrap_or(Value::Null);
            Some(EngineEvent {
                event: event.to_owned(),
                payload,
            })
        }
        None => None,
    }
}

/// Extract every complete SSE frame (`\n\n`-delimited) from `buffer`,
/// leaving any trailing partial frame for the next chunk.
fn drain_frames(buffer: &mut String) -> Vec<SseFrame> {
    let mut out = Vec::new();
    while let Some(idx) = buffer.find("\n\n") {
        // `idx` addresses the found two-byte delimiter, so `idx + 2` is
        // within the buffer; saturating is its total spelling.
        let block: String = buffer.drain(..idx.saturating_add(2)).collect();
        if let Some(frame) = parse_frame(&block) {
            out.push(frame);
        }
    }
    out
}

/// Parse one `\n\n`-delimited SSE block. Comment lines (`:`-prefixed
/// keep-alives) are skipped; a block with no `event` and no `data` yields
/// `None`.
fn parse_frame(block: &str) -> Option<SseFrame> {
    let mut id = None;
    let mut event = None;
    let mut data_lines: Vec<String> = Vec::new();
    for line in block.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "id" => id = value.trim().parse::<u64>().ok(),
            "event" => event = Some(value.to_string()),
            "data" => data_lines.push(value.to_string()),
            _ => {}
        }
    }
    if event.is_none() && data_lines.is_empty() {
        return None;
    }
    Some(SseFrame {
        id,
        event,
        data: data_lines.join("\n"),
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    use super::*;

    // --- scripted SSE server -------------------------------------------------
    //
    // A raw TCP stand-in for rp's `/api/events/subscribe`: each accepted
    // connection captures the request head (for `last-event-id`
    // assertions), plays one scripted response, and either closes or
    // holds the stream open until the client hangs up.

    struct Connection {
        status: &'static str,
        body: &'static str,
        /// Hold the stream open after `body` until the client closes.
        hold: bool,
    }

    async fn spawn_sse_server(
        script: Vec<Connection>,
    ) -> (String, mpsc::UnboundedReceiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (heads_tx, heads_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            for connection in script {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match socket.read(&mut byte).await {
                        Ok(1..) => head.push(byte[0]),
                        _ => break,
                    }
                }
                let _ = heads_tx.send(String::from_utf8_lossy(&head).into_owned());
                let response = format!(
                    "HTTP/1.1 {}\r\ncontent-type: text/event-stream\r\n\
                     connection: close\r\n\r\n{}",
                    connection.status, connection.body
                );
                if socket.write_all(response.as_bytes()).await.is_err() {
                    continue;
                }
                if connection.hold {
                    let mut sink = [0u8; 64];
                    loop {
                        match socket.read(&mut sink).await {
                            Ok(0) | Err(_) => break,
                            _ => {}
                        }
                    }
                }
            }
        });
        (format!("http://{addr}/api/events/subscribe"), heads_rx)
    }

    async fn next_event(intake: &mut EventIntake) -> EngineEvent {
        timeout(Duration::from_secs(5), intake.next())
            .await
            .expect("expected an event within 5s")
    }

    // --- client behavior -------------------------------------------------------

    #[tokio::test]
    async fn test_envelopes_are_forwarded_in_order_and_control_frames_are_not() {
        let body = ": keep-alive\n\n\
                    id: 7\nevent: exposure_started\ndata: {\"event\":\"exposure_started\",\
                    \"event_seq\":7,\"payload\":{\"camera_id\":\"main-cam\"}}\n\n\
                    event: stream_gap\ndata: {\"event\":\"stream_gap\",\"lagged\":3}\n\n\
                    id: 8\nevent: exposure_complete\ndata: {\"event\":\"exposure_complete\",\
                    \"event_seq\":8,\"payload\":{\"document_id\":\"doc-1\"}}\n\n\
                    event: stream_error\ndata: {\"error\":\"serialization\"}\n\n";
        let (url, _heads) = spawn_sse_server(vec![Connection {
            status: "200 OK",
            body,
            hold: true,
        }])
        .await;

        let mut intake = subscribe(url, &RpConnection::default()).await;
        assert_eq!(
            next_event(&mut intake).await,
            EngineEvent {
                event: "exposure_started".to_owned(),
                payload: json!({ "camera_id": "main-cam" }),
            }
        );
        assert_eq!(
            next_event(&mut intake).await,
            EngineEvent {
                event: "exposure_complete".to_owned(),
                payload: json!({ "document_id": "doc-1" }),
            }
        );
        // The control frames were consumed, not forwarded.
        assert!(intake.try_next().is_none());
    }

    #[tokio::test]
    async fn test_reconnect_resumes_from_the_last_seen_event_seq() {
        let first = "id: 41\nevent: tick\ndata: {\"payload\":{\"n\":1}}\n\n\
                     id: 42\nevent: tick\ndata: {\"payload\":{\"n\":2}}\n\n";
        let second = "id: 43\nevent: tick\ndata: {\"payload\":{\"n\":3}}\n\n";
        let (url, mut heads) = spawn_sse_server(vec![
            Connection {
                status: "200 OK",
                body: first,
                hold: false,
            },
            Connection {
                status: "200 OK",
                body: second,
                hold: true,
            },
        ])
        .await;

        let mut intake = subscribe(url, &RpConnection::default()).await;
        for n in 1..=3 {
            assert_eq!(next_event(&mut intake).await.payload, json!({ "n": n }));
        }
        let first_head = heads.recv().await.unwrap().to_lowercase();
        assert!(
            !first_head.contains("last-event-id"),
            "the initial connect must not carry a cursor: {first_head}"
        );
        let second_head = heads.recv().await.unwrap().to_lowercase();
        assert!(
            second_head.contains("last-event-id: 42"),
            "the reconnect must resume after the last seen event_seq: {second_head}"
        );
    }

    #[tokio::test]
    async fn test_subscribe_returns_with_the_stream_already_connected() {
        // The trigger/`until_event` contract needs the subscription live
        // before the session's first instruction: an event emitted
        // milliseconds into the first tool call must be captured.
        let (url, mut heads) = spawn_sse_server(vec![Connection {
            status: "200 OK",
            body: "",
            hold: true,
        }])
        .await;

        let _intake = subscribe(url, &RpConnection::default()).await;
        assert!(
            heads.try_recv().is_ok(),
            "subscribe must complete the initial connect before returning"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_a_server_that_accepts_but_never_answers_does_not_hang_subscribe() {
        // A wedged endpoint: the OS accepts the TCP connection (into the
        // listener's backlog) but no response headers ever come. reqwest's
        // `connect_timeout` would never fire here — only the attempt-level
        // timeout gets `subscribe` off the `/invoke` path. Paused tokio
        // time auto-advances past the cap, so this is instant.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let mut intake = timeout(
            Duration::from_mins(1),
            subscribe(
                format!("http://{addr}/api/events/subscribe"),
                &RpConnection::default(),
            ),
        )
        .await
        .expect("subscribe must time out a wedged endpoint, not hang the /invoke path");
        assert!(intake.try_next().is_none());
        drop(listener);
    }

    #[tokio::test]
    async fn test_a_rejected_subscribe_is_retried() {
        let (url, _heads) = spawn_sse_server(vec![
            Connection {
                status: "503 Service Unavailable",
                body: "",
                hold: false,
            },
            Connection {
                status: "200 OK",
                body: "id: 1\nevent: tick\ndata: {\"payload\":null}\n\n",
                hold: true,
            },
        ])
        .await;

        let mut intake = subscribe(url, &RpConnection::default()).await;
        assert_eq!(next_event(&mut intake).await.event, "tick");
    }

    #[tokio::test]
    async fn test_dropping_the_intake_ends_the_client() {
        let (url, mut heads) = spawn_sse_server(vec![
            Connection {
                status: "200 OK",
                body: "",
                hold: true,
            },
            // A second scripted connection that must never be used.
            Connection {
                status: "200 OK",
                body: "",
                hold: true,
            },
        ])
        .await;

        let intake = subscribe(url, &RpConnection::default()).await;
        // First connection established…
        heads.recv().await.unwrap();
        drop(intake);
        // …and no reconnect after the intake is gone (well past the
        // reconnect backoff).
        tokio::time::sleep(RECONNECT_BACKOFF + Duration::from_millis(500)).await;
        assert!(
            heads.try_recv().is_err(),
            "the client must stop once the intake is dropped"
        );
    }

    // --- frame parsing ---------------------------------------------------------

    #[test]
    fn test_parse_frame_reads_id_event_and_data() {
        let frame = parse_frame("id: 12\nevent: slew_started\ndata: {\"a\":1}\n\n").unwrap();
        assert_eq!(frame.id, Some(12));
        assert_eq!(frame.event.as_deref(), Some("slew_started"));
        assert_eq!(frame.data, "{\"a\":1}");
    }

    #[test]
    fn test_parse_frame_handles_crlf_and_missing_space_after_colon() {
        let frame = parse_frame("id:3\r\nevent:tick\r\ndata:{}\r\n\r\n").unwrap();
        assert_eq!(frame.id, Some(3));
        assert_eq!(frame.event.as_deref(), Some("tick"));
        assert_eq!(frame.data, "{}");
    }

    #[test]
    fn test_parse_frame_joins_multiple_data_lines() {
        let frame = parse_frame("data: line1\ndata: line2\n\n").unwrap();
        assert_eq!(frame.data, "line1\nline2");
    }

    #[test]
    fn test_parse_frame_skips_comment_only_blocks() {
        assert!(parse_frame(": keep-alive\n\n").is_none());
    }

    #[test]
    fn test_drain_frames_leaves_a_trailing_partial_frame() {
        let mut buffer = "event: a\ndata: {}\n\nevent: b\nda".to_owned();
        let frames = drain_frames(&mut buffer);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("a"));
        assert_eq!(buffer, "event: b\nda");
    }

    // --- envelope classification -------------------------------------------------

    fn frame(event: Option<&str>, data: &str) -> SseFrame {
        SseFrame {
            id: None,
            event: event.map(str::to_owned),
            data: data.to_owned(),
        }
    }

    #[test]
    fn test_engine_event_extracts_the_payload() {
        let event = engine_event(frame(
            Some("exposure_complete"),
            "{\"event\":\"exposure_complete\",\"payload\":{\"document_id\":\"d\"}}",
        ))
        .unwrap();
        assert_eq!(event.event, "exposure_complete");
        assert_eq!(event.payload, json!({ "document_id": "d" }));
    }

    #[test]
    fn test_engine_event_defaults_a_missing_payload_to_null() {
        let event = engine_event(frame(Some("session_started"), "{\"event_seq\":1}")).unwrap();
        assert_eq!(event.payload, Value::Null);
    }

    #[test]
    fn test_engine_event_drops_control_and_malformed_frames() {
        assert!(engine_event(frame(Some("stream_gap"), "{\"lagged\":9}")).is_none());
        assert!(engine_event(frame(Some("stream_error"), "{}")).is_none());
        assert!(engine_event(frame(Some("tick"), "not json")).is_none());
        assert!(engine_event(frame(None, "{\"payload\":1}")).is_none());
    }

    fn authed_connection(ca_cert: Option<&str>) -> RpConnection {
        RpConnection {
            service_auth: Some(rp_mcp_client::ClientAuthConfig {
                username: "observatory".to_owned(),
                password: "secret".to_owned(),
            }),
            ca_cert: ca_cert.map(std::path::PathBuf::from),
        }
    }

    #[test]
    fn test_an_unusable_ca_pin_withholds_the_credential() {
        // The CA path is configured but unreadable: the client falls back
        // to default trust, and the credential must fall away with the
        // pin — it is never paired with a client that lost it.
        let connection = authed_connection(Some("/nonexistent/ca.pem"));
        let (_client, auth) = event_client(&connection);
        assert!(auth.is_none(), "credential must be withheld");
    }

    #[test]
    fn test_a_buildable_client_keeps_the_credential_for_the_policy_check() {
        // No CA configured: the default client builds fine and the
        // credential passes through — `basic_authorization` downstream
        // still refuses to emit a header without a CA.
        let connection = authed_connection(None);
        let (_client, auth) = event_client(&connection);
        assert!(auth.is_some());
        let header =
            rp_mcp_client::basic_authorization("https://localhost:1/x", auth, None).unwrap();
        assert!(header.is_none(), "no CA, no header");
    }
}

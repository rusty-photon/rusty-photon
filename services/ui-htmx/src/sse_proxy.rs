//! `/stream/events` — the stateless SSE proxy between the browser and rp's
//! `GET /api/events/subscribe` (see `docs/services/ui-htmx.md` "The SSE proxy").
//!
//! Contract (the stream page and the BDD suite are written against this):
//!
//! - The handler forwards the browser's `Last-Event-ID` header to rp; absent,
//!   it subscribes from cursor `0` so rp's retained history replays and
//!   populates the feed.
//! - Each rp event envelope becomes up to two BFF SSE frames: `event: feed`
//!   (the rendered card, `pages::stream::feed_card`) plus the strip-slot
//!   frames the event warrants (`pages::stream::slot_updates`). The **final**
//!   frame of the group carries `id:` = the envelope's `event_seq`, so the
//!   browser's cursor only advances past fully-delivered envelopes
//!   (at-least-once on a torn delivery).
//! - rp's `stream_gap` frames (no `id`) render as a feed divider, still no `id`.
//! - On rp connect failure or stream loss the proxy pushes an
//!   `event: operation` "rp unreachable" slot fragment and **ends the stream**;
//!   the browser's `EventSource` auto-reconnects with its cursor.
//! - Frames are `:keep-alive`d every 15 s, and every proxy stream ends promptly
//!   when the service-wide shutdown token on [`AppState`](crate::AppState)
//!   fires (axum's graceful shutdown does not end open SSE responses on its
//!   own — axum #2673, the hazard the `test-sse` spike pinned; rp's own SSE
//!   endpoint uses the same `sse_shutdown` pattern).

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use maud::Markup;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::pages::stream::{self, EventEnvelope};
use crate::{AppState, RpState};

/// Interval between BFF-side `:keep-alive` comments (independent of rp's own).
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// `GET /stream/events`.
pub async fn events(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(rp) = state.rp() else {
        debug!("SSE proxy requested but no rp target is configured");
        return (
            StatusCode::NOT_FOUND,
            "no rp orchestrator is configured; the activity stream is unavailable",
        )
            .into_response();
    };
    let rp = Arc::clone(rp);
    let cursor = browser_cursor(&headers);
    let shutdown = state.sse_shutdown().clone();

    let body = async_stream::stream! {
        let mut response = match connect(&rp, cursor, &shutdown).await {
            Ok(response) => response,
            Err(ConnectFailure::Shutdown) => return,
            Err(ConnectFailure::Unreachable) => {
                // One status-slot frame, then end: the browser's EventSource
                // reconnects with its cursor, so retry/backoff and replay come
                // from the platform + rp rather than BFF state.
                yield Ok::<Event, Infallible>(unreachable_event());
                return;
            }
        };
        let mut parser = FrameParser::default();
        loop {
            let chunk = tokio::select! {
                () = shutdown.cancelled() => {
                    debug!("SSE proxy stream ending: service shutdown");
                    return;
                }
                chunk = response.chunk() => chunk,
            };
            match chunk {
                Ok(Some(bytes)) => {
                    for frame in parser.push(&bytes) {
                        for event in translate(&frame) {
                            yield Ok(event);
                        }
                    }
                }
                Ok(None) => {
                    debug!("rp event stream ended; closing the proxy stream for a browser reconnect");
                    yield Ok(unreachable_event());
                    return;
                }
                Err(e) => {
                    debug!("rp event stream read failed: {e}; closing the proxy stream");
                    yield Ok(unreachable_event());
                    return;
                }
            }
        }
    };
    Sse::new(body)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response()
}

/// The browser's replay cursor: its `last-event-id` header (set automatically
/// by `EventSource` on reconnect) parsed as `u64`; absent or garbage → `0`
/// (subscribe from the start of rp's retained history).
fn browser_cursor(headers: &HeaderMap) -> u64 {
    headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Why a subscribe attempt produced no rp stream.
enum ConnectFailure {
    /// rp refused, rejected, or errored — surface "unreachable" and end.
    Unreachable,
    /// The service is shutting down — end silently.
    Shutdown,
}

/// One long-lived subscribe to rp's `GET /api/events/subscribe`: CA-trusting
/// client, optional Basic credentials, and the cursor as `last-event-id`. No
/// request timeout — the stream is long-lived by design; the shutdown token
/// (select!ed here and in the read loop) and the client disconnect bound it.
async fn connect(
    rp: &RpState,
    cursor: u64,
    shutdown: &CancellationToken,
) -> Result<reqwest::Response, ConnectFailure> {
    let url = format!("{}/api/events/subscribe", rp.base_url);
    let mut request = rp
        .stream_client
        .get(&url)
        .header("accept", "text/event-stream")
        .header("last-event-id", cursor.to_string());
    if let Some((username, password)) = &rp.stream_auth {
        request = request.basic_auth(username, Some(password));
    }
    let sent = tokio::select! {
        () = shutdown.cancelled() => {
            debug!("SSE proxy connect abandoned: service shutdown");
            return Err(ConnectFailure::Shutdown);
        }
        sent = request.send() => sent,
    };
    match sent {
        Ok(response) if response.status().is_success() => {
            debug!(%url, cursor, "connected to rp's event stream");
            Ok(response)
        }
        Ok(response) => {
            debug!(%url, status = %response.status(), "rp rejected the event-stream subscribe");
            Err(ConnectFailure::Unreachable)
        }
        Err(e) => {
            debug!(%url, error = %e, "rp event-stream connect failed");
            Err(ConnectFailure::Unreachable)
        }
    }
}

/// The `event: operation` "rp unreachable — retrying…" slot frame pushed
/// before the stream ends on any rp-side failure.
fn unreachable_event() -> Event {
    Event::default()
        .event("operation")
        .data(fragment_data(stream::unreachable_slot()))
}

/// Render a fragment for an SSE `data:` field. axum's `Event::data` splits
/// embedded `\n` across multiple `data:` lines (rejoined by the browser), but
/// panics on a raw `\r` — which could reach us through an rp payload string —
/// so carriage returns are stripped.
fn fragment_data(markup: Markup) -> String {
    markup.into_string().replace('\r', "")
}

/// Translate one parsed rp frame into the BFF frames it warrants (see the
/// module contract). Frames that are neither an envelope, a `stream_gap`, nor
/// a `stream_error` are skipped (logged at `debug!`) — rp's stream is the
/// source of truth and the proxy must not invent cards for noise.
fn translate(frame: &SseFrame) -> Vec<Event> {
    // rp's synthetic stream_error frame (an envelope failed to serialize) may
    // carry no data at all — detect it by name and render the warn card.
    if frame.event.as_deref() == Some("stream_error") {
        debug!("rp sent a stream_error control frame");
        return vec![Event::default()
            .event("feed")
            .data(fragment_data(stream::stream_error_card(&frame.data)))];
    }
    let Ok(value) = serde_json::from_str::<Value>(&frame.data) else {
        debug!(
            frame_id = ?frame.id,
            frame_event = ?frame.event,
            "skipping an rp SSE frame with non-JSON data"
        );
        return Vec::new();
    };
    // stream_gap is a control shape, not an envelope: a feed divider,
    // mirroring rp's own id-less gap frame (it must not advance the cursor).
    if value.get("event").and_then(Value::as_str) == Some("stream_gap") {
        return vec![Event::default()
            .event("feed")
            .data(fragment_data(stream::gap_divider(&value)))];
    }
    let envelope: EventEnvelope = match serde_json::from_value(value) {
        Ok(envelope) => envelope,
        Err(e) => {
            debug!(
                frame_id = ?frame.id,
                frame_event = ?frame.event,
                "skipping an rp SSE frame that is not an event envelope: {e}"
            );
            return Vec::new();
        }
    };
    let mut events: Vec<Event> = stream::slot_updates(&envelope)
        .into_iter()
        .map(|(slot, markup)| Event::default().event(slot).data(fragment_data(markup)))
        .collect();
    // The feed frame is the LAST of the group and alone carries the `id`, so
    // the browser's cursor only advances past a fully-delivered envelope.
    events.push(
        Event::default()
            .event("feed")
            .data(fragment_data(stream::feed_card(&envelope)))
            .id(envelope.event_seq.to_string()),
    );
    events
}

// --- the incremental SSE frame parser --------------------------------------------

/// One complete SSE frame from rp's stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseFrame {
    /// The `id:` line, verbatim (rp sends the envelope's `event_seq`; the
    /// proxy's outgoing `id` comes from the envelope itself, so this is kept
    /// for diagnostics).
    pub(crate) id: Option<String>,
    /// The `event:` line — rp's event-type name.
    pub(crate) event: Option<String>,
    /// The `data:` lines, joined with `\n`. Empty when the frame carried none
    /// (rp's degenerate `stream_error` frame is an `event:` line alone).
    pub(crate) data: String,
}

/// Incremental parser for the SSE line protocol: push raw byte chunks, get
/// every frame the chunk completed. Frames may be split at **any** byte
/// boundary (including inside a multi-byte UTF-8 character — lines are
/// extracted at `\n` bytes, which never occur inside a UTF-8 sequence, and
/// decoded per complete line); a trailing unterminated frame stays buffered
/// until its blank line arrives. Handles LF and CRLF endings, multi-line
/// `data:` (joined with `\n`), `:`-prefixed comment lines (rp's
/// `:keep-alive`s), and a missing space after the field colon.
#[derive(Debug, Default)]
pub(crate) struct FrameParser {
    /// Bytes of the current, not-yet-terminated line.
    partial_line: Vec<u8>,
    /// Fields of the current, not-yet-terminated frame.
    id: Option<String>,
    event: Option<String>,
    data_lines: Vec<String>,
    /// Whether the current frame has seen any `id:`/`event:`/`data:` field —
    /// a blank line after nothing but comments dispatches no frame.
    seen_field: bool,
}

impl FrameParser {
    /// Feed one chunk of bytes; returns every frame it completed, in order.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        for &byte in chunk {
            if byte == b'\n' {
                let line = String::from_utf8_lossy(&self.partial_line).into_owned();
                self.partial_line.clear();
                if let Some(frame) = self.line_complete(line.trim_end_matches('\r')) {
                    frames.push(frame);
                }
            } else {
                self.partial_line.push(byte);
            }
        }
        frames
    }

    /// Process one complete line; returns a frame when the line was the
    /// frame-terminating blank line.
    fn line_complete(&mut self, line: &str) -> Option<SseFrame> {
        if line.is_empty() {
            return self.take_frame();
        }
        if line.starts_with(':') {
            return None; // a comment (rp's `:keep-alive` heartbeats)
        }
        let (field, value) = match line.split_once(':') {
            // Per the SSE spec, exactly one leading space is stripped.
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            // A line with no colon is a field name with an empty value.
            None => (line, ""),
        };
        match field {
            "id" => self.id = Some(value.to_string()),
            "event" => self.event = Some(value.to_string()),
            "data" => self.data_lines.push(value.to_string()),
            // Unknown fields (e.g. `retry:`) are ignored per the SSE spec.
            _ => return None,
        }
        self.seen_field = true;
        None
    }

    fn take_frame(&mut self) -> Option<SseFrame> {
        if !self.seen_field {
            return None;
        }
        self.seen_field = false;
        Some(SseFrame {
            id: self.id.take(),
            event: self.event.take(),
            data: std::mem::take(&mut self.data_lines).join("\n"),
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::http::header;
    use axum::routing::get;
    use axum::Router;
    use serde_json::Value;
    use tokio::time::timeout;

    use super::*;
    use crate::driver_client::{
        ConfigApplyResponse, ConfigClient, ConfigClientError, ConfigGetResponse,
        ConfigSchemaResponse,
    };
    use crate::rp_client::MockRpApi;

    /// Every await in these tests is bounded — a wedged stream must fail the
    /// test, not hang the suite.
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    // --- the incremental parser ---------------------------------------------

    fn frames_of(chunks: &[&[u8]]) -> Vec<SseFrame> {
        let mut parser = FrameParser::default();
        let mut frames = Vec::new();
        for chunk in chunks {
            frames.extend(parser.push(chunk));
        }
        frames
    }

    #[test]
    fn parser_reads_a_complete_frame() {
        let frames = frames_of(&[b"id: 7\nevent: slew_started\ndata: {\"a\":1}\n\n"]);
        assert_eq!(
            frames,
            vec![SseFrame {
                id: Some("7".to_string()),
                event: Some("slew_started".to_string()),
                data: "{\"a\":1}".to_string(),
            }]
        );
    }

    #[test]
    fn parser_handles_every_split_point() {
        // The frame includes a multi-byte character (é) so a split inside a
        // UTF-8 sequence is exercised too.
        let full: &[u8] =
            "id: 12\nevent: exposure_started\ndata: {\"camera\":\"h\u{e9}\"}\n\n".as_bytes();
        for split in 0..=full.len() {
            let frames = frames_of(&[&full[..split], &full[split..]]);
            assert_eq!(frames.len(), 1, "split at byte {split}");
            assert_eq!(frames[0].id.as_deref(), Some("12"), "split at byte {split}");
            assert_eq!(
                frames[0].event.as_deref(),
                Some("exposure_started"),
                "split at byte {split}"
            );
            assert_eq!(
                frames[0].data, "{\"camera\":\"h\u{e9}\"}",
                "split at byte {split}"
            );
        }
    }

    #[test]
    fn parser_handles_crlf_and_mixed_line_endings() {
        let frames = frames_of(&[b"id: 2\r\nevent: tick\ndata: a\r\n\r\n"]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].id.as_deref(), Some("2"));
        assert_eq!(frames[0].event.as_deref(), Some("tick"));
        assert_eq!(frames[0].data, "a");
    }

    #[test]
    fn parser_returns_multiple_frames_from_one_chunk() {
        let frames = frames_of(&[b"event: a\ndata: 1\n\nevent: b\ndata: 2\n\n"]);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event.as_deref(), Some("a"));
        assert_eq!(frames[1].event.as_deref(), Some("b"));
        // Fields must not leak between frames.
        assert_eq!(frames[1].id, None);
        assert_eq!(frames[1].data, "2");
    }

    #[test]
    fn parser_keeps_a_trailing_unterminated_frame_buffered() {
        let mut parser = FrameParser::default();
        let frames = parser.push(b"event: a\ndata: 1\n\nevent: b\nda");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("a"));
        // The tail completes on the next chunk.
        let frames = parser.push(b"ta: 2\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("b"));
        assert_eq!(frames[0].data, "2");
    }

    #[test]
    fn parser_joins_multi_line_data_with_newlines() {
        let frames = frames_of(&[b"data: line1\ndata: line2\n\n"]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "line1\nline2");
    }

    #[test]
    fn parser_skips_comment_lines_and_comment_only_blocks() {
        // A comment-only block dispatches nothing…
        assert_eq!(frames_of(&[b": keep-alive\n\n"]), Vec::<SseFrame>::new());
        // …and a comment inside a frame doesn't disturb its fields.
        let frames = frames_of(&[b"event: x\n: heartbeat\ndata: y\n\n"]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("x"));
        assert_eq!(frames[0].data, "y");
    }

    #[test]
    fn parser_strips_at_most_one_leading_space_after_the_colon() {
        let frames = frames_of(&[b"data:x\n\ndata:  y\n\n"]);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].data, "x");
        assert_eq!(frames[1].data, " y");
    }

    #[test]
    fn parser_emits_a_frame_with_an_event_and_no_data() {
        // rp's degenerate stream_error frame is an `event:` line alone.
        let frames = frames_of(&[b"event: stream_error\n\n"]);
        assert_eq!(
            frames,
            vec![SseFrame {
                id: None,
                event: Some("stream_error".to_string()),
                data: String::new(),
            }]
        );
    }

    #[test]
    fn parser_ignores_unknown_fields_and_blank_line_runs() {
        let frames = frames_of(&[b"retry: 100\nevent: x\ndata: y\n\n\n\nretry: 5\n\n"]);
        // The retry-only block dispatches nothing; blank-line runs are inert.
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("x"));
    }

    // --- proxy-level tests against an in-test axum stub (ADR-004) --------------

    /// A `ConfigClient` the proxy never calls (state plumbing only).
    struct UnusedConfigClient;

    #[async_trait::async_trait]
    impl ConfigClient for UnusedConfigClient {
        async fn get_config(&self) -> Result<ConfigGetResponse, ConfigClientError> {
            Err(ConfigClientError::Transport("unused".to_string()))
        }
        async fn get_schema(&self) -> Result<ConfigSchemaResponse, ConfigClientError> {
            Err(ConfigClientError::Transport("unused".to_string()))
        }
        async fn apply_config(
            &self,
            _config: &Value,
        ) -> Result<ConfigApplyResponse, ConfigClientError> {
            Err(ConfigClientError::Transport("unused".to_string()))
        }
    }

    /// rp-target state pointed at an in-test stub base URL.
    fn state_for(stub_base_url: &str) -> AppState {
        AppState::with_rp_parts(
            Arc::new(UnusedConfigClient),
            Arc::new(MockRpApi::new()),
            Arc::new(crate::probe::MockProbeHttp::new()),
        )
        .with_rp_base_url(stub_base_url)
    }

    /// Serve `app` on 127.0.0.1:0; returns its base URL.
    async fn spawn_router(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// The BFF under test: the real router over the given state.
    async fn spawn_bff(state: AppState) -> String {
        spawn_router(crate::build_router(state)).await
    }

    /// An rp stub whose `/api/events/subscribe` answers with a canned, finite
    /// SSE body and records the `last-event-id` header it received.
    async fn spawn_recording_stub(body: String) -> (String, Arc<Mutex<Option<String>>>) {
        let captured = Arc::new(Mutex::new(None));
        let seen = Arc::clone(&captured);
        let app = Router::new().route(
            "/api/events/subscribe",
            get(move |headers: HeaderMap| {
                let seen = Arc::clone(&seen);
                let body = body.clone();
                async move {
                    *seen.lock().unwrap() = headers
                        .get("last-event-id")
                        .and_then(|value| value.to_str().ok())
                        .map(String::from);
                    ([(header::CONTENT_TYPE, "text/event-stream")], body)
                }
            }),
        );
        (spawn_router(app).await, captured)
    }

    /// GET the BFF's `/stream/events` and collect the whole (finite) body.
    async fn collect_stream(bff_base_url: &str, last_event_id: Option<&str>) -> String {
        let client = reqwest::Client::new();
        let mut request = client.get(format!("{bff_base_url}/stream/events"));
        if let Some(id) = last_event_id {
            request = request.header("last-event-id", id);
        }
        let mut response = timeout(TEST_TIMEOUT, request.send())
            .await
            .expect("connect timed out")
            .expect("connect failed");
        assert_eq!(response.status().as_u16(), 200);
        let mut collected = String::new();
        loop {
            match timeout(TEST_TIMEOUT, response.chunk())
                .await
                .expect("read timed out")
                .expect("read failed")
            {
                Some(chunk) => collected.push_str(&String::from_utf8_lossy(&chunk)),
                None => return collected,
            }
        }
    }

    /// The non-empty SSE blocks of a collected body.
    fn blocks(collected: &str) -> Vec<&str> {
        collected
            .split("\n\n")
            .filter(|block| !block.trim().is_empty())
            .collect()
    }

    fn has_id_line(block: &str) -> bool {
        block.lines().any(|line| line.starts_with("id:"))
    }

    fn envelope_json(event: &str, seq: u64, payload: &str) -> String {
        format!(
            r#"{{"event_id":"e-{seq}","event_seq":{seq},"event":"{event}","timestamp":"2026-07-10T21:18:04Z","payload":{payload}}}"#
        )
    }

    /// A canned rp session: a keep-alive comment, two envelopes, a lag gap,
    /// and a degenerate `stream_error` frame — then the body ends.
    fn scripted_body() -> String {
        format!(
            ": keep-alive\n\nid: 41\nevent: exposure_started\ndata: {}\n\nid: 42\nevent: safety_changed\ndata: {}\n\nevent: stream_gap\ndata: {{\"event\":\"stream_gap\",\"lagged\":3}}\n\nevent: stream_error\n\n",
            envelope_json(
                "exposure_started",
                41,
                r#"{"camera_id":"main-cam","duration":"5s"}"#
            ),
            envelope_json(
                "safety_changed",
                42,
                r#"{"monitor":"weather-watcher","new_state":"unsafe"}"#
            ),
        )
    }

    /// The full translation pipeline over a scripted rp stream: slot frames
    /// precede each feed frame, the `id` rides the feed frame alone, gap and
    /// `stream_error` frames stay id-less, and the stream ends with the
    /// unreachable operation frame when rp's body ends.
    #[tokio::test]
    async fn proxy_translates_a_scripted_rp_stream_in_order() {
        let (stub, _captured) = spawn_recording_stub(scripted_body()).await;
        let bff = spawn_bff(state_for(&stub)).await;
        let collected = collect_stream(&bff, None).await;
        let blocks = blocks(&collected);
        assert_eq!(blocks.len(), 7, "unexpected frame set:\n{collected}");

        // exposure_started: operation slot (no id), then the feed card with
        // id 41 as its LAST line group member.
        assert!(blocks[0].contains("event: operation"), "{}", blocks[0]);
        assert!(
            blocks[0].contains("Exposing · main-cam · 5s"),
            "{}",
            blocks[0]
        );
        assert!(!has_id_line(blocks[0]), "{}", blocks[0]);
        assert!(blocks[1].contains("event: feed"), "{}", blocks[1]);
        assert!(
            blocks[1].contains("Exposure started · main-cam · 5s"),
            "{}",
            blocks[1]
        );
        assert!(blocks[1].contains("feed-card sev-live"), "{}", blocks[1]);
        assert!(blocks[1].contains("21:18:04"), "{}", blocks[1]);
        assert!(
            blocks[1].lines().any(|line| line == "id: 41"),
            "{}",
            blocks[1]
        );

        // safety_changed: safety chip slot, then the feed card with id 42.
        assert!(blocks[2].contains("event: safety"), "{}", blocks[2]);
        assert!(blocks[2].contains("UNSAFE"), "{}", blocks[2]);
        assert!(!has_id_line(blocks[2]), "{}", blocks[2]);
        assert!(blocks[3].contains("event: feed"), "{}", blocks[3]);
        assert!(blocks[3].contains("Safety: UNSAFE"), "{}", blocks[3]);
        assert!(
            blocks[3].lines().any(|line| line == "id: 42"),
            "{}",
            blocks[3]
        );

        // The lag gap: an id-less feed divider.
        assert!(blocks[4].contains("event: feed"), "{}", blocks[4]);
        assert!(blocks[4].contains("feed-gap"), "{}", blocks[4]);
        assert!(blocks[4].contains("lagged by 3"), "{}", blocks[4]);
        assert!(!has_id_line(blocks[4]), "{}", blocks[4]);

        // The degenerate stream_error frame: an id-less warn card.
        assert!(blocks[5].contains("event: feed"), "{}", blocks[5]);
        assert!(blocks[5].contains("Stream error"), "{}", blocks[5]);
        assert!(blocks[5].contains("sev-warn"), "{}", blocks[5]);
        assert!(!has_id_line(blocks[5]), "{}", blocks[5]);

        // rp's body ended → the unreachable operation frame, then stream end.
        assert!(blocks[6].contains("event: operation"), "{}", blocks[6]);
        assert!(
            blocks[6].contains("rp unreachable — retrying…"),
            "{}",
            blocks[6]
        );
    }

    /// (a) The browser's cursor reaches the stub: absent → 0, present →
    /// forwarded, garbage → 0.
    #[tokio::test]
    async fn proxy_forwards_the_browser_cursor_to_rp() {
        let (stub, captured) = spawn_recording_stub(String::new()).await;
        let bff = spawn_bff(state_for(&stub)).await;

        collect_stream(&bff, None).await;
        assert_eq!(captured.lock().unwrap().as_deref(), Some("0"));

        collect_stream(&bff, Some("41")).await;
        assert_eq!(captured.lock().unwrap().as_deref(), Some("41"));

        collect_stream(&bff, Some("not-a-number")).await;
        assert_eq!(captured.lock().unwrap().as_deref(), Some("0"));
    }

    /// (c) A refused connection yields the operation-slot frame then stream end.
    #[tokio::test]
    async fn proxy_reports_a_refused_rp_and_ends_the_stream() {
        // Bind and drop a listener so the port is (momentarily) refusing.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let bff = spawn_bff(state_for(&format!("http://{addr}"))).await;
        let collected = collect_stream(&bff, None).await;
        let blocks = blocks(&collected);
        assert_eq!(blocks.len(), 1, "{collected}");
        assert!(blocks[0].contains("event: operation"), "{collected}");
        assert!(
            blocks[0].contains("rp unreachable — retrying…"),
            "{collected}"
        );
    }

    /// A non-2xx subscribe answer is "unreachable" too.
    #[tokio::test]
    async fn proxy_reports_a_non_2xx_rp_and_ends_the_stream() {
        let app = Router::new().route(
            "/api/events/subscribe",
            get(|| async { (StatusCode::SERVICE_UNAVAILABLE, "down") }),
        );
        let stub = spawn_router(app).await;
        let bff = spawn_bff(state_for(&stub)).await;
        let collected = collect_stream(&bff, None).await;
        let blocks = blocks(&collected);
        assert_eq!(blocks.len(), 1, "{collected}");
        assert!(blocks[0].contains("event: operation"), "{collected}");
        assert!(blocks[0].contains("rp unreachable"), "{collected}");
    }

    /// (d) The eviction-shaped `stream_gap` becomes an id-less feed divider.
    #[tokio::test]
    async fn proxy_translates_an_eviction_gap_without_an_id() {
        let body = "event: stream_gap\ndata: {\"event\":\"stream_gap\",\"requested_after\":5,\"oldest_available\":9}\n\n".to_string();
        let (stub, _captured) = spawn_recording_stub(body).await;
        let bff = spawn_bff(state_for(&stub)).await;
        let collected = collect_stream(&bff, None).await;
        let blocks = blocks(&collected);
        assert_eq!(blocks.len(), 2, "{collected}");
        assert!(blocks[0].contains("event: feed"), "{collected}");
        assert!(blocks[0].contains("feed-gap"), "{collected}");
        assert!(blocks[0].contains("requested after 5"), "{collected}");
        assert!(blocks[0].contains("oldest available 9"), "{collected}");
        assert!(!has_id_line(blocks[0]), "{collected}");
        // …then the end-of-stream unreachable frame.
        assert!(blocks[1].contains("rp unreachable"), "{collected}");
    }

    /// A data frame that is neither an envelope nor a control shape is skipped.
    #[tokio::test]
    async fn proxy_skips_frames_that_are_not_envelopes() {
        let body =
            "event: mystery\ndata: {\"no_event_field\":true}\n\nevent: mystery\ndata: not json\n\n"
                .to_string();
        let (stub, _captured) = spawn_recording_stub(body).await;
        let bff = spawn_bff(state_for(&stub)).await;
        let collected = collect_stream(&bff, None).await;
        let blocks = blocks(&collected);
        // Only the end-of-stream unreachable frame comes out.
        assert_eq!(blocks.len(), 1, "{collected}");
        assert!(blocks[0].contains("rp unreachable"), "{collected}");
    }

    /// No rp target → 404, not a stream.
    #[tokio::test]
    async fn proxy_without_rp_target_is_404() {
        let state = AppState::with_client("dsd-fp2", Arc::new(UnusedConfigClient));
        let bff = spawn_bff(state).await;
        let response = timeout(TEST_TIMEOUT, reqwest::get(format!("{bff}/stream/events")))
            .await
            .expect("connect timed out")
            .expect("connect failed");
        assert_eq!(response.status().as_u16(), 404);
    }

    /// The shutdown token ends an open proxy stream (axum #2673: graceful
    /// shutdown alone would not) — the wiring `main` relies on.
    #[tokio::test]
    async fn shutdown_token_ends_an_open_proxy_stream() {
        // A stub that pushes one envelope and then holds the stream open.
        let envelope = envelope_json("safety_changed", 1, r#"{"new_state":"safe"}"#);
        let app = Router::new().route(
            "/api/events/subscribe",
            get(move || {
                let envelope = envelope.clone();
                async move {
                    let stream = async_stream::stream! {
                        yield Ok::<Event, Infallible>(
                            Event::default()
                                .event("safety_changed")
                                .data(envelope)
                                .id("1"),
                        );
                        std::future::pending::<()>().await;
                    };
                    Sse::new(stream)
                }
            }),
        );
        let stub = spawn_router(app).await;
        let state = state_for(&stub);
        let token = state.sse_shutdown().clone();
        let bff = spawn_bff(state).await;

        let client = reqwest::Client::new();
        let mut response = timeout(
            TEST_TIMEOUT,
            client.get(format!("{bff}/stream/events")).send(),
        )
        .await
        .expect("connect timed out")
        .expect("connect failed");

        // Wait until the envelope's frames arrive, so the stream is live.
        let mut collected = String::new();
        while !collected.contains("id: 1") {
            let chunk = timeout(TEST_TIMEOUT, response.chunk())
                .await
                .expect("read timed out")
                .expect("read failed")
                .expect("stream ended before the envelope arrived");
            collected.push_str(&String::from_utf8_lossy(&chunk));
        }

        token.cancel();

        // The proxy stream must END (not hang): drain to None within bounds.
        while timeout(TEST_TIMEOUT, response.chunk())
            .await
            .expect("stream did not end after the shutdown token fired")
            .expect("read failed")
            .is_some()
        {}
    }
}

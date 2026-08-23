//! In-process SSE client for asserting on rp's `/api/events/subscribe` stream.
//!
//! The pull-side counterpart to [`super::WebhookReceiver`] (the push side).
//! It opens a long-lived `GET /api/events/subscribe`, reads the response body
//! chunk by chunk with [`reqwest::Response::chunk`] (no `stream` cargo feature
//! needed — that is only required for `bytes_stream()`), parses the SSE
//! `id:`/`event:`/`data:` line protocol into [`SseFrame`]s, and stores them in
//! a shared vec the test asserts against. See
//! `services/rp/src/routes.rs::subscribe_events` for the wire contract.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

/// One parsed SSE frame from rp's event stream.
#[derive(Debug, Clone)]
pub struct SseFrame {
    /// The SSE `id:` line — the envelope's `event_seq`. `None` on `stream_gap`
    /// frames, which deliberately carry no id so they never become a reconnect
    /// cursor.
    pub id: Option<u64>,
    /// The SSE `event:` line — the event type (e.g. `"slew_started"`,
    /// `"stream_gap"`).
    pub event: Option<String>,
    /// The SSE `data:` payload — the full `EventEnvelope` JSON (or the
    /// `stream_gap` diagnostic body).
    pub data: String,
    /// `data` parsed once at construction (`Null` if it doesn't parse), so the
    /// accessors below read fields without re-parsing the payload per call.
    parsed: Value,
}

impl SseFrame {
    /// The parsed `data` payload (`Null` if it didn't parse).
    #[must_use]
    pub const fn json(&self) -> &Value {
        &self.parsed
    }

    /// The envelope `operation_id`, if present in `data`.
    pub fn operation_id(&self) -> Option<String> {
        self.parsed
            .get("operation_id")
            .and_then(|v| v.as_str())
            .map(String::from)
    }

    /// The `event` type, falling back to the `data` envelope's `event` field.
    #[must_use]
    pub fn event_type(&self) -> Option<String> {
        self.event.clone().or_else(|| {
            self.parsed
                .get("event")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
    }
}

/// A connected SSE client reading rp's event stream into a shared buffer.
///
/// Dropping the client aborts its reader task and closes the HTTP connection,
/// which lets rp's stream handler end (and keeps it from blocking rp's
/// graceful shutdown — see `docs/skills/testing.md` §5.4).
pub struct SseClient {
    frames: Arc<RwLock<Vec<SseFrame>>>,
    task: JoinHandle<()>,
}

impl SseClient {
    /// Open `GET {base_url}/api/events/subscribe`, optionally sending
    /// `Last-Event-ID: {last_event_id}` to replay from history on reconnect.
    /// Returns once the server has answered `200` — so the server-side
    /// subscription is live and every subsequent emission is captured — then
    /// reads the body in a background task.
    ///
    /// # Panics
    ///
    /// Panics if the subscribe request cannot be sent or the server answers
    /// anything but `200`.
    pub async fn connect(base_url: &str, last_event_id: Option<u64>) -> Self {
        let url = format!("{base_url}/api/events/subscribe");
        let client = reqwest::Client::new();
        let mut req = client.get(&url).header("accept", "text/event-stream");
        if let Some(id) = last_event_id {
            req = req.header("last-event-id", id.to_string());
        }
        let resp = req.send().await.expect("SSE subscribe request failed");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "GET /api/events/subscribe must answer 200"
        );

        let frames: Arc<RwLock<Vec<SseFrame>>> = Arc::new(RwLock::new(Vec::new()));
        let frames_task = frames.clone();
        let task = tokio::spawn(async move {
            let mut resp = resp;
            let mut buffer = String::new();
            // Reads until the server closes the body (end of stream) or a
            // transport error — both end the `while let`.
            while let Ok(Some(chunk)) = resp.chunk().await {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                let parsed = drain_frames(&mut buffer);
                if !parsed.is_empty() {
                    frames_task.write().await.extend(parsed);
                }
            }
        });

        Self { frames, task }
    }

    /// Snapshot the frames received so far.
    pub async fn frames(&self) -> Vec<SseFrame> {
        self.frames.read().await.clone()
    }

    /// The highest SSE `id` (`event_seq`) seen so far — the reconnect cursor a
    /// client would resend as `Last-Event-ID`.
    pub async fn max_event_seq(&self) -> Option<u64> {
        self.frames.read().await.iter().filter_map(|f| f.id).max()
    }

    /// Poll up to ~10 s for a frame whose event type matches `event_type`,
    /// returning the first match (or `None` on timeout).
    pub async fn wait_for_event(&self, event_type: &str) -> Option<SseFrame> {
        for _ in 0..100 {
            if let Some(frame) = self
                .frames
                .read()
                .await
                .iter()
                .find(|f| f.event_type().as_deref() == Some(event_type))
            {
                return Some(frame.clone());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    }
}

impl Drop for SseClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Extract every complete SSE frame (delimited by a blank line) from `buffer`,
/// leaving any trailing partial frame in place for the next chunk to complete.
fn drain_frames(buffer: &mut String) -> Vec<SseFrame> {
    let mut out = Vec::new();
    while let Some(idx) = buffer.find("\n\n") {
        let block: String = buffer.drain(..idx.saturating_add(2)).collect();
        if let Some(frame) = parse_frame(&block) {
            out.push(frame);
        }
    }
    out
}

/// Parse one `\n\n`-delimited SSE block into a frame. Comment lines
/// (`:`-prefixed, e.g. keep-alives) are skipped; a block with no `event` and
/// no `data` (a bare keep-alive) yields `None` per the SSE spec.
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
    let data = data_lines.join("\n");
    let parsed = serde_json::from_str(&data).unwrap_or(Value::Null);
    Some(SseFrame {
        id,
        event,
        data,
        parsed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_complete_event_frame() {
        let mut buf =
            "event: slew_started\nid: 12\ndata: {\"event_seq\":12,\"operation_id\":\"op-1\"}\n\n"
                .to_string();
        let frames = drain_frames(&mut buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].id, Some(12));
        assert_eq!(frames[0].event.as_deref(), Some("slew_started"));
        assert_eq!(frames[0].operation_id().as_deref(), Some("op-1"));
        assert!(buf.is_empty(), "a fully-consumed frame leaves no remainder");
    }

    #[test]
    fn keeps_a_partial_frame_for_the_next_chunk() {
        let mut buf = "event: slew_started\nid: 12\n".to_string();
        assert!(drain_frames(&mut buf).is_empty(), "no blank line yet");
        buf.push_str("data: {\"event_seq\":12}\n\n");
        let frames = drain_frames(&mut buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].id, Some(12));
    }

    #[test]
    fn skips_keep_alive_comment_frames() {
        let mut buf =
            ":keep-alive\n\nevent: park_started\nid: 3\ndata: {\"event_seq\":3}\n\n".to_string();
        let frames = drain_frames(&mut buf);
        assert_eq!(frames.len(), 1, "the keep-alive comment is dropped");
        assert_eq!(frames[0].event.as_deref(), Some("park_started"));
    }

    #[test]
    fn stream_gap_frame_has_no_id() {
        let mut buf =
            "event: stream_gap\ndata: {\"event\":\"stream_gap\",\"lagged\":44}\n\n".to_string();
        let frames = drain_frames(&mut buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].id, None, "stream_gap carries no reconnect cursor");
        assert_eq!(frames[0].event.as_deref(), Some("stream_gap"));
    }
}

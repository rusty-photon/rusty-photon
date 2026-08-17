//! The activity stream (`/stream`) — the chosen mock (`7-stream-fold.html`)
//! rendered live from rp's event stream (see `docs/services/ui-htmx.md`
//! "Activity stream").
//!
//! DOM contract (the SSE proxy, the BDD suite, and app.css are written
//! against this):
//!
//! - The page declares `hx-ext="sse" sse-connect="/stream/events"` once, on a
//!   wrapper containing every swap region.
//! - `#feed` has `sse-swap="feed" hx-swap="afterbegin"`: each event card
//!   prepends. Cards are `article.feed-card` with a severity modifier
//!   (`sev-ok` / `sev-bad` / `sev-live` / `sev-warn`), an `.evt-title`, an
//!   optional `.evt-detail`, and a `time.mono` stamp. `stream_gap` renders a
//!   `div.feed-gap` divider instead of a card.
//! - The sticky strip `#status-strip` (inside the fold header) carries three
//!   slots updated by named SSE events: `#slot-operation` (`sse-swap="operation"`),
//!   `#slot-guide` (`sse-swap="guide"`), `#slot-session` (`sse-swap="session"`).
//! - The fold panel (CSS Grid `0fr → 1fr`, checkbox `#fold-state` + label — no
//!   JavaScript) contains `#equipment-leds`, which re-fetches
//!   `/stream/equipment` via `hx-get` + `hx-trigger="every 10s"`.
//! - Without an rp target the page renders the shared "no rp configured" card.
//!
//! The htmx SSE extension swaps a slot's **innerHTML**, so the slot fragments
//! ([`slot_updates`], [`unreachable_slot`]) are the slot's *inner* markup only —
//! the page-rendered `#slot-*` element (with its `sse-swap` attribute) stays in
//! place and keeps matching subsequent frames.

use axum::extract::State;
use maud::{html, Markup};
use serde::Deserialize;
use serde_json::Value;
use tracing::debug;

use crate::pages::{layout_with_nav, NavTab};
use crate::rp_client::EquipmentStatus;
use crate::AppState;

/// Page title for the stream routes.
const TITLE: &str = "rusty-photon · activity";

// --- the envelope mirror ---------------------------------------------------

/// BFF-side mirror of rp's event envelope (`services/rp/src/events.rs`),
/// reduced to the fields the renderers consume. The remaining envelope fields
/// (`event_id`, `operation_id`, `started_at`, `ended_at`,
/// `predicted_duration_ms`, `max_duration_ms`, and anything rp adds later) are
/// tolerated and ignored — serde's default for unknown fields — and `payload`
/// stays a raw [`Value`] so rendering is **total**: a missing payload field
/// renders as an omission, never an error.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct EventEnvelope {
    /// The event type string, e.g. `"slew_started"`.
    pub(crate) event: String,
    /// The bus's monotonic per-emission counter — the SSE replay cursor. The
    /// proxy puts it on the **last** frame of each envelope's group as `id:`.
    pub(crate) event_seq: u64,
    /// Emission timestamp, `%Y-%m-%dT%H:%M:%SZ`.
    #[serde(default)]
    pub(crate) timestamp: String,
    /// Wall-clock operation duration; present on terminal events.
    #[serde(default)]
    pub(crate) elapsed_ms: Option<u64>,
    /// Operation-specific detail: inputs on `*_started`, outcomes on terminals.
    #[serde(default)]
    pub(crate) payload: Value,
}

// --- severity ----------------------------------------------------------------

/// Feed-card severity, per the module-doc DOM contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Ok,
    Bad,
    Live,
    Warn,
}

impl Severity {
    const fn class(self) -> &'static str {
        match self {
            Self::Ok => "sev-ok",
            Self::Bad => "sev-bad",
            Self::Live => "sev-live",
            Self::Warn => "sev-warn",
        }
    }
}

/// Classify an envelope: `*_failed` and `safety_changed:unsafe` are bad,
/// `*_complete`/`*_settled` ok, `*_started` live, `stream_error` and
/// `guide_stopped` warn; other point events are neutral.
fn severity_of(env: &EventEnvelope) -> Severity {
    match env.event.as_str() {
        "safety_changed" => {
            if env.payload.get("new_state").and_then(Value::as_str) == Some("unsafe") {
                Severity::Bad
            } else {
                Severity::Ok
            }
        }
        "stream_error" | "guide_stopped" => Severity::Warn,
        event if event.ends_with("_failed") => Severity::Bad,
        event if event.ends_with("_complete") || event.ends_with("_settled") => Severity::Ok,
        event if event.ends_with("_started") => Severity::Live,
        _ => Severity::Ok,
    }
}

// --- payload display helpers ---------------------------------------------------

/// A payload field as display text: strings verbatim, numbers via
/// [`num_display`], booleans as `true`/`false`. Absent, null, or nested values
/// are omissions (`None`) — rendering never fails on a missing field.
fn field(payload: &Value, key: &str) -> Option<String> {
    match payload.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(num_display(n)),
        Some(Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    }
}

/// Compact display for a JSON number: integers verbatim, floats trimmed to at
/// most 4 decimals (a raw f64 repr like `10.684583333333333` doesn't belong in
/// a feed card).
fn num_display(n: &serde_json::Number) -> String {
    match n.as_f64() {
        Some(f) if n.is_f64() => {
            let s = format!("{f:.4}");
            let s = s.trim_end_matches('0').trim_end_matches('.');
            if s.is_empty() || s == "-" {
                "0".to_string()
            } else {
                s.to_string()
            }
        }
        _ => n.to_string(),
    }
}

/// `Some("{label} {value}")` when `key` is present in the payload.
fn labeled(label: &str, payload: &Value, key: &str) -> Option<String> {
    field(payload, key).map(|v| format!("{label} {v}"))
}

/// The `RA … · Dec …` detail parts for whichever of the two keys are present.
fn coords(payload: &Value, ra_key: &str, dec_key: &str) -> Vec<String> {
    [
        labeled("RA", payload, ra_key),
        labeled("Dec", payload, dec_key),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// The `{"error": "…"}` detail every `*_failed` payload carries.
fn error_detail(payload: &Value) -> Vec<String> {
    field(payload, "error").into_iter().collect()
}

/// The guide/dither settle outcome: `RMS 0.66 px · RA 0.42 · Dec 0.51 · n=120`.
fn rms_detail(payload: &Value) -> Vec<String> {
    [
        field(payload, "total_rms_px").map(|v| format!("RMS {v} px")),
        labeled("RA", payload, "rms_ra_px"),
        labeled("Dec", payload, "rms_dec_px"),
        field(payload, "sample_count").map(|v| format!("n={v}")),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// The compact payload dump for events the BFF doesn't know — canonical
/// (sorted-key) serialization so the output is deterministic. Null and empty
/// payloads are omissions (`{}` on a card is noise, not information).
fn payload_dump(payload: &Value) -> Vec<String> {
    let empty = match payload {
        Value::Null => true,
        Value::Object(map) => map.is_empty(),
        _ => false,
    };
    if empty {
        Vec::new()
    } else {
        vec![super::canonical_json(payload)]
    }
}

/// `"{base} · {extra} · …"` for the extras that are present.
fn join_parts(base: &str, extras: &[Option<String>]) -> String {
    let mut out = base.to_string();
    for extra in extras.iter().flatten() {
        out.push_str(" · ");
        out.push_str(extra);
    }
    out
}

// --- per-event title + detail ---------------------------------------------------

/// The human title and payload-specific detail parts for one envelope, per the
/// rp event catalog (`docs/services/ui-htmx.md` "Activity stream"). Unknown
/// event names degrade to a humanized name + compact payload dump rather than
/// vanishing.
fn card_text(env: &EventEnvelope) -> (String, Vec<String>) {
    let p = &env.payload;
    match env.event.as_str() {
        "slew_started" => ("Slew started".to_string(), coords(p, "ra", "dec")),
        "slew_complete" => {
            let mut detail = coords(p, "actual_ra", "actual_dec");
            if detail.is_empty() {
                detail = coords(p, "ra", "dec");
            }
            ("Slew complete".to_string(), detail)
        }
        "slew_failed" => ("Slew failed".to_string(), error_detail(p)),
        "move_focuser_started" => (
            join_parts("Focuser move started", &[field(p, "focuser_id")]),
            field(p, "position")
                .map(|pos| format!("→ {pos}"))
                .into_iter()
                .collect(),
        ),
        "move_focuser_complete" => (
            join_parts("Focuser move complete", &[field(p, "focuser_id")]),
            labeled("position", p, "position").into_iter().collect(),
        ),
        "move_focuser_failed" => (
            join_parts("Focuser move failed", &[field(p, "focuser_id")]),
            error_detail(p),
        ),
        "park_started" => ("Park started".to_string(), Vec::new()),
        "park_complete" => ("Park complete".to_string(), Vec::new()),
        "park_failed" => ("Park failed".to_string(), error_detail(p)),
        "unpark_started" => ("Unpark started".to_string(), Vec::new()),
        "unpark_complete" => ("Unpark complete".to_string(), Vec::new()),
        "unpark_failed" => ("Unpark failed".to_string(), error_detail(p)),
        "sync_mount_complete" => ("Mount sync complete".to_string(), coords(p, "ra", "dec")),
        "sync_mount_failed" => ("Mount sync failed".to_string(), error_detail(p)),
        "exposure_started" => (
            join_parts(
                "Exposure started",
                &[field(p, "camera_id"), field(p, "duration")],
            ),
            Vec::new(),
        ),
        "exposure_complete" => (
            "Exposure complete".to_string(),
            field(p, "file_path")
                .or_else(|| field(p, "document_id"))
                .into_iter()
                .collect(),
        ),
        "exposure_failed" => ("Exposure failed".to_string(), error_detail(p)),
        "focus_started" => (
            join_parts("Autofocus started", &[field(p, "camera_id")]),
            [
                labeled("focuser", p, "focuser_id"),
                labeled("position", p, "position"),
                field(p, "temperature").map(|t| format!("{t}°C")),
            ]
            .into_iter()
            .flatten()
            .collect(),
        ),
        "focus_complete" => (
            join_parts("Autofocus complete", &[field(p, "camera_id")]),
            [
                labeled("position", p, "position"),
                labeled("HFR", p, "hfr"),
                field(p, "samples_used").map(|n| format!("{n} samples")),
            ]
            .into_iter()
            .flatten()
            .collect(),
        ),
        "focus_failed" => ("Autofocus failed".to_string(), error_detail(p)),
        "centering_started" => (
            join_parts("Centering started", &[field(p, "camera_id")]),
            coords(p, "ra", "dec")
                .into_iter()
                .chain(field(p, "tolerance_arcsec").map(|t| format!("tolerance {t}″")))
                .chain(field(p, "max_attempts").map(|n| format!("≤ {n} attempts")))
                .collect(),
        ),
        "centering_iteration" => (
            join_parts("Centering iteration", &[field(p, "camera_id")]),
            [
                field(p, "residual_arcsec").map(|r| format!("residual {r}″")),
                field(p, "action"),
            ]
            .into_iter()
            .flatten()
            .chain(coords(p, "solved_ra", "solved_dec"))
            .collect(),
        ),
        "centering_complete" => (
            join_parts("Centering complete", &[field(p, "camera_id")]),
            [
                field(p, "final_error_arcsec").map(|e| format!("error {e}″")),
                field(p, "attempts").map(|n| format!("{n} attempts")),
            ]
            .into_iter()
            .flatten()
            .chain(coords(p, "final_ra", "final_dec"))
            .collect(),
        ),
        "centering_failed" => ("Centering failed".to_string(), error_detail(p)),
        "plate_solve_started" => (
            "Plate solve started".to_string(),
            field(p, "image_path").into_iter().collect(),
        ),
        "plate_solve_complete" => (
            "Plate solve complete".to_string(),
            [
                labeled("RA", p, "ra_center"),
                labeled("Dec", p, "dec_center"),
                field(p, "pixel_scale_arcsec").map(|s| format!("{s}″/px")),
                field(p, "rotation_deg").map(|r| format!("rotation {r}°")),
                field(p, "solver"),
            ]
            .into_iter()
            .flatten()
            .collect(),
        ),
        "plate_solve_failed" => ("Plate solve failed".to_string(), error_detail(p)),
        "guide_started" => ("Guiding started".to_string(), Vec::new()),
        "guide_settled" => ("Guiding settled".to_string(), rms_detail(p)),
        "guide_failed" => ("Guiding failed".to_string(), error_detail(p)),
        "guide_stopped" => (
            "Guiding stopped".to_string(),
            field(p, "reason").into_iter().collect(),
        ),
        "dither_started" => ("Dither started".to_string(), Vec::new()),
        "dither_settled" => ("Dither settled".to_string(), rms_detail(p)),
        "dither_failed" => ("Dither failed".to_string(), error_detail(p)),
        "filter_switch" => (
            join_parts("Filter switch", &[field(p, "filter_name")]),
            field(p, "filter_wheel_id").into_iter().collect(),
        ),
        "safety_changed" => {
            let title = match p.get("new_state").and_then(Value::as_str) {
                Some("safe") => "Safety: SAFE".to_string(),
                Some("unsafe") => "Safety: UNSAFE".to_string(),
                _ => "Safety changed".to_string(),
            };
            (
                title,
                labeled("monitor", p, "monitor").into_iter().collect(),
            )
        }
        "session_started" => (
            "Session started".to_string(),
            [
                labeled("workflow", p, "workflow_id"),
                labeled("session", p, "session_id"),
            ]
            .into_iter()
            .flatten()
            .collect(),
        ),
        "session_stopped" => (
            "Session stopped".to_string(),
            [field(p, "reason"), labeled("workflow", p, "workflow_id")]
                .into_iter()
                .flatten()
                .collect(),
        ),
        "document_persistence_failed" => (
            "Document persistence failed".to_string(),
            [field(p, "file_path"), field(p, "error")]
                .into_iter()
                .flatten()
                .collect(),
        ),
        "stream_error" => ("Stream error".to_string(), payload_dump(p)),
        other => (super::humanize(other), payload_dump(p)),
    }
}

// --- fragment renderers (used by the SSE proxy) ---------------------------------

/// One feed card for an rp event envelope: severity class, human title,
/// payload-specific detail (mono), and the `HH:MM:SS` timestamp. When
/// `elapsed_ms` is present a humanized duration joins the detail line.
pub(crate) fn feed_card(env: &EventEnvelope) -> Markup {
    let (title, mut detail) = card_text(env);
    if let Some(ms) = env.elapsed_ms {
        detail.push(humanize_ms(ms));
    }
    html! {
        article class=(format!("feed-card {}", severity_of(env).class())) {
            span.evt-title { (title) }
            time.mono { (short_time(&env.timestamp)) }
            @if !detail.is_empty() { div.evt-detail { (detail.join(" · ")) } }
        }
    }
}

/// The `stream_gap` feed divider ("events were missed"), from the raw gap JSON
/// (`{"event":"stream_gap","requested_after":N,"oldest_available":M}` or
/// `{"event":"stream_gap","lagged":N}` — gaps are not envelopes).
pub(crate) fn gap_divider(gap: &Value) -> Markup {
    let requested_after = gap.get("requested_after").and_then(Value::as_u64);
    let oldest_available = gap.get("oldest_available").and_then(Value::as_u64);
    let lagged = gap.get("lagged").and_then(Value::as_u64);
    let note = match (requested_after, oldest_available, lagged) {
        (Some(after), Some(oldest), _) => {
            format!("events were missed — requested after {after}, oldest available {oldest}")
        }
        (_, _, Some(n)) => format!("events were missed — the stream lagged by {n} events"),
        _ => "events were missed".to_string(),
    };
    html! { div.feed-gap { (note) } }
}

/// The warn card for rp's synthetic `stream_error` frame (emitted when an
/// envelope failed to serialize; it may carry no data at all).
pub(crate) fn stream_error_card(detail: &str) -> Markup {
    html! {
        article class="feed-card sev-warn" {
            span.evt-title { "Stream error" }
            @if !detail.is_empty() { div.evt-detail { (detail) } }
        }
    }
}

/// The operation-slot inner fragment pushed when rp is unreachable (also the
/// page's initial slot content when the render-time fetches failed). The
/// browser's `EventSource` auto-reconnects, so "retrying" is literal.
pub(crate) fn unreachable_slot() -> Markup {
    html! { "rp unreachable — retrying…" }
}

/// The strip-slot fragments an envelope warrants, as `(sse event name, inner
/// markup)` pairs: `operation` on every `*_started` and every terminal,
/// `guide` on `guide_settled`/`dither_settled` (the RMS readout), `session` on
/// `session_started`/`session_stopped`/`safety_changed` (state chips). Point
/// events (`filter_switch`, `centering_iteration`, `guide_stopped`,
/// `document_persistence_failed`) leave the slots alone — the proxy is
/// stateless, so an untouched slot keeps its previous content.
pub(crate) fn slot_updates(env: &EventEnvelope) -> Vec<(&'static str, Markup)> {
    match env.event.as_str() {
        "session_started" => vec![("session", session_state_chip("active"))],
        "session_stopped" => vec![("session", session_state_chip("stopped"))],
        "safety_changed" => vec![("session", safety_chip(&env.payload))],
        // The settle events are the guide/dither success terminators: they
        // update the RMS readout *and* close out the operation slot.
        "guide_settled" | "dither_settled" => vec![
            ("guide", guide_slot(&env.payload)),
            ("operation", operation_slot(&card_text(env).0)),
        ],
        // A background point event, not the current operation.
        "document_persistence_failed" => Vec::new(),
        event if event.ends_with("_started") => {
            vec![("operation", operation_slot(&operation_started_label(env)))]
        }
        event if event.ends_with("_complete") || event.ends_with("_failed") => {
            vec![("operation", operation_slot(&card_text(env).0))]
        }
        _ => Vec::new(),
    }
}

/// The operation slot's inner fragment (plain text).
fn operation_slot(text: &str) -> Markup {
    html! { (text) }
}

/// The in-progress operation label for a `*_started` event ("Slewing…",
/// "Exposing · main-cam · 5s", …).
fn operation_started_label(env: &EventEnvelope) -> String {
    let p = &env.payload;
    match env.event.as_str() {
        "slew_started" => "Slewing…".to_string(),
        "park_started" => "Parking…".to_string(),
        "unpark_started" => "Unparking…".to_string(),
        "plate_solve_started" => "Plate solving…".to_string(),
        "guide_started" => "Starting guiding…".to_string(),
        "dither_started" => "Dithering…".to_string(),
        "exposure_started" => {
            join_parts("Exposing", &[field(p, "camera_id"), field(p, "duration")])
        }
        "focus_started" => join_parts("Focusing", &[field(p, "camera_id")]),
        "centering_started" => join_parts("Centering", &[field(p, "camera_id")]),
        "move_focuser_started" => {
            let mut label = join_parts("Moving focuser", &[field(p, "focuser_id")]);
            if let Some(position) = field(p, "position") {
                use std::fmt::Write as _;
                let _ = write!(label, " → {position}");
            }
            label
        }
        other => super::humanize(other),
    }
}

/// The guide slot's inner fragment: `RMS 0.42 px · n=120`.
fn guide_slot(payload: &Value) -> Markup {
    let parts: Vec<String> = [
        field(payload, "total_rms_px").map(|v| format!("RMS {v} px")),
        field(payload, "sample_count").map(|v| format!("n={v}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        html! { "RMS —" }
    } else {
        html! { (parts.join(" · ")) }
    }
}

/// The session-state chip (`session idle` / `session active` /
/// `session interrupted` / `session stopped`), also used for the page's
/// initial render from `GET /api/session/status`.
fn session_state_chip(state: &str) -> Markup {
    let class = match state {
        "active" => "chip live",
        "interrupted" => "chip warn",
        // idle / stopped / unknown all read as "not running".
        _ => "chip muted",
    };
    html! { span class=(class) { "session " (state) } }
}

/// The SAFE / UNSAFE chip for `safety_changed`.
fn safety_chip(payload: &Value) -> Markup {
    match payload.get("new_state").and_then(Value::as_str) {
        Some("safe") => html! { span.chip.ok { "SAFE" } },
        Some("unsafe") => html! { span.chip.bad { "UNSAFE" } },
        other => html! { span.chip.warn { "safety " (other.unwrap_or("changed")) } },
    }
}

// --- small formatting helpers ----------------------------------------------------

/// `HH:MM:SS` from a `%Y-%m-%dT%H:%M:%SZ` timestamp by string slicing (chars
/// 11..19), falling back to the raw string when it doesn't fit that shape. No
/// chrono dependency — the stamp is display-only.
fn short_time(timestamp: &str) -> &str {
    timestamp.get(11..19).unwrap_or(timestamp)
}

/// Humanize a millisecond duration via humantime: `400ms`, `5s`, `2m 4s`,
/// `1h 2m 3s`.
fn humanize_ms(ms: u64) -> String {
    humantime::format_duration(std::time::Duration::from_millis(ms)).to_string()
}

// --- the page shell ---------------------------------------------------------------

/// `GET /stream` — the page shell. The strip and LED panel render from
/// `GET /api/session/status` + `GET /api/equipment` (fetched concurrently,
/// both best-effort: a failure renders the operation slot as "rp unreachable —
/// retrying…" and the LED panel with a note, and the page still renders); the
/// feed starts empty and fills from the SSE replay.
pub async fn page(State(state): State<AppState>) -> Markup {
    let Some(rp) = state.rp() else {
        return layout_with_nav(
            TITLE,
            NavTab::Activity,
            &super::equipment::no_rp_card("the activity stream"),
        );
    };
    let (session, equipment) = tokio::join!(rp.api.session_status(), rp.api.equipment_status());
    let session = match session {
        Ok(session) => Some(session),
        Err(e) => {
            debug!("stream page: session status fetch failed: {e}");
            None
        }
    };
    let equipment = match equipment {
        Ok(equipment) => Some(equipment),
        Err(e) => {
            debug!("stream page: equipment status fetch failed: {e}");
            None
        }
    };
    layout_with_nav(
        TITLE,
        NavTab::Activity,
        &shell(session.as_deref(), equipment.as_ref()),
    )
}

/// The stream page body: the SSE wrapper, the sticky fold strip (slots), the
/// fold panel (equipment LEDs), and the (initially empty) feed.
fn shell(session: Option<&str>, equipment: Option<&EquipmentStatus>) -> Markup {
    let rp_reachable = session.is_some() && equipment.is_some();
    html! {
        // Load the htmx SSE extension AFTER htmx core (which `layout` puts in
        // <head>, so it runs first). This body <script> executes during parsing
        // — before htmx's initial scan — so the extension is registered by the
        // time htmx processes `hx-ext="sse"` (mirrors `sse_fixtures`).
        script src="/assets/htmx-ext-sse.js" {}
        div #stream-page hx-ext="sse" sse-connect="/stream/events" {
            // The fold: a checkbox + label drive the CSS Grid 0fr→1fr panel
            // animation — no JavaScript (the chosen mock's pattern). Visually
            // hidden, not `hidden`, so it stays keyboard-focusable; the
            // aria-label names it (the label's text is the whole status strip).
            input
                type="checkbox"
                id="fold-state"
                class="fold-state visually-hidden"
                aria-label="Toggle the equipment panel";
            label for="fold-state" class="fold-strip" {
                span.strip-pulse {}
                div #status-strip {
                    span #slot-operation .strip-label sse-swap="operation" {
                        @if rp_reachable { "Idle" } @else { (unreachable_slot()) }
                    }
                    span.sep { "│" }
                    div.stat {
                        "guide"
                        span #slot-guide .v sse-swap="guide" { "RMS —" }
                    }
                    span.sep { "│" }
                    span #slot-session sse-swap="session" {
                        (session_state_chip(session.unwrap_or("unknown")))
                    }
                }
                span.grow {}
                span.fold-toggle { span.lbl {} span.chev { "▾" } }
            }
            div.fold-body {
                div.panel {
                    div.panel-head { span.title { "Equipment" } }
                    (equipment_leds(equipment))
                }
            }
            div #feed sse-swap="feed" hx-swap="afterbegin" {}
        }
    }
}

/// `GET /stream/equipment` — the fold panel's equipment-LED fragment (also the
/// htmx poll target: the fragment carries its own `hx-get`/`hx-trigger`, so
/// each 10s poll replaces it wholesale and a down rp self-heals).
pub async fn equipment_fragment(State(state): State<AppState>) -> Markup {
    let status = match state.rp() {
        None => None,
        Some(rp) => match rp.api.equipment_status().await {
            Ok(status) => Some(status),
            Err(e) => {
                debug!("equipment LED poll failed: {e}");
                None
            }
        },
    };
    equipment_leds(status.as_ref())
}

/// The `#equipment-leds` fragment: one LED per configured device (`None`
/// status → a dim note; the htmx poll keeps retrying).
fn equipment_leds(status: Option<&EquipmentStatus>) -> Markup {
    html! {
        div #equipment-leds hx-get="/stream/equipment" hx-trigger="every 10s"
            hx-swap="outerHTML" {
            @match status {
                None => p.dim-note { "rp unreachable — equipment state unknown" }
                Some(status) => (led_list(status))
            }
        }
    }
}

fn led_list(status: &EquipmentStatus) -> Markup {
    let mut items: Vec<(&'static str, Option<&str>, bool)> = Vec::new();
    for (kind, list) in [
        ("camera", &status.cameras),
        ("filter wheel", &status.filter_wheels),
        ("cover calibrator", &status.cover_calibrators),
        ("focuser", &status.focusers),
        ("safety monitor", &status.safety_monitors),
    ] {
        for device in list {
            items.push((kind, Some(device.id.as_str()), device.connected));
        }
    }
    if let Some(mount) = &status.mount {
        items.push(("mount", None, mount.connected));
    }
    if items.is_empty() {
        return html! { p.dim-note { "No equipment configured." } };
    }
    html! {
        ul {
            @for (kind, id, connected) in &items {
                li {
                    span class=(if *connected { "led ok" } else { "led bad" }) {}
                    span.kind { (kind) }
                    @if let Some(id) = id { span.dev-id { (id) } }
                }
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unreachable)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::driver_client::{
        ConfigApplyResponse, ConfigClient, ConfigClientError, ConfigGetResponse,
        ConfigSchemaResponse,
    };
    use crate::rp_client::{DeviceStatus, MockRpApi, MountStatus};

    fn envelope(event: &str, payload: Value) -> EventEnvelope {
        EventEnvelope {
            event: event.to_string(),
            event_seq: 7,
            timestamp: "2026-07-10T21:18:04Z".to_string(),
            elapsed_ms: None,
            payload,
        }
    }

    fn card(event: &str, payload: Value) -> String {
        feed_card(&envelope(event, payload)).into_string()
    }

    // --- the feed-card catalog table -------------------------------------------

    struct CardCase {
        event: &'static str,
        payload: Value,
        severity: &'static str,
        title: &'static str,
        detail: Option<&'static str>,
    }

    fn case(
        event: &'static str,
        payload: Value,
        severity: &'static str,
        title: &'static str,
        detail: Option<&'static str>,
    ) -> CardCase {
        CardCase {
            event,
            payload,
            severity,
            title,
            detail,
        }
    }

    /// One case per event family from the rp catalog: severity class, human
    /// title, payload detail, and the sliced HH:MM:SS timestamp.
    #[test]
    fn feed_card_renders_every_catalog_family() {
        let cases = vec![
            case(
                "slew_started",
                json!({"ra": 10.5, "dec": 41.25}),
                "sev-live",
                "Slew started",
                Some("RA 10.5 · Dec 41.25"),
            ),
            case(
                "slew_complete",
                json!({"ra": 10.5, "dec": 41.25, "actual_ra": 10.51, "actual_dec": 41.26}),
                "sev-ok",
                "Slew complete",
                Some("RA 10.51 · Dec 41.26"),
            ),
            case(
                "slew_failed",
                json!({"error": "mount fault"}),
                "sev-bad",
                "Slew failed",
                Some("mount fault"),
            ),
            case(
                "move_focuser_started",
                json!({"focuser_id": "foc-1", "position": 5210}),
                "sev-live",
                "Focuser move started · foc-1",
                Some("→ 5210"),
            ),
            case(
                "move_focuser_complete",
                json!({"focuser_id": "foc-1", "position": 5210}),
                "sev-ok",
                "Focuser move complete · foc-1",
                Some("position 5210"),
            ),
            case(
                "move_focuser_failed",
                json!({"error": "stalled"}),
                "sev-bad",
                "Focuser move failed",
                Some("stalled"),
            ),
            case("park_started", json!({}), "sev-live", "Park started", None),
            case("park_complete", json!({}), "sev-ok", "Park complete", None),
            case(
                "park_failed",
                json!({"error": "limit"}),
                "sev-bad",
                "Park failed",
                Some("limit"),
            ),
            case(
                "unpark_started",
                json!({}),
                "sev-live",
                "Unpark started",
                None,
            ),
            case(
                "unpark_failed",
                json!({"error": "no power"}),
                "sev-bad",
                "Unpark failed",
                Some("no power"),
            ),
            case(
                "sync_mount_complete",
                json!({"ra": 1.5, "dec": -2.25}),
                "sev-ok",
                "Mount sync complete",
                Some("RA 1.5 · Dec -2.25"),
            ),
            case(
                "sync_mount_failed",
                json!({"error": "rejected"}),
                "sev-bad",
                "Mount sync failed",
                Some("rejected"),
            ),
            case(
                "exposure_started",
                json!({"camera_id": "main-cam", "duration": "5s"}),
                "sev-live",
                "Exposure started · main-cam · 5s",
                None,
            ),
            case(
                "exposure_complete",
                json!({"document_id": "doc-9", "file_path": "/data/l-042.fits"}),
                "sev-ok",
                "Exposure complete",
                Some("/data/l-042.fits"),
            ),
            case(
                "exposure_failed",
                json!({"error": "camera gone"}),
                "sev-bad",
                "Exposure failed",
                Some("camera gone"),
            ),
            case(
                "focus_started",
                json!({"camera_id": "main-cam", "focuser_id": "foc-1", "position": 5000, "temperature": -3.5}),
                "sev-live",
                "Autofocus started · main-cam",
                Some("focuser foc-1 · position 5000 · -3.5°C"),
            ),
            case(
                "focus_complete",
                json!({"camera_id": "main-cam", "focuser_id": "foc-1", "position": 5210, "hfr": 2.14, "samples_used": 9}),
                "sev-ok",
                "Autofocus complete · main-cam",
                Some("position 5210 · HFR 2.14 · 9 samples"),
            ),
            case(
                "focus_failed",
                json!({"error": "no stars"}),
                "sev-bad",
                "Autofocus failed",
                Some("no stars"),
            ),
            case(
                "centering_started",
                json!({"camera_id": "main-cam", "ra": 10.5, "dec": 41.2, "tolerance_arcsec": 30, "max_attempts": 3}),
                "sev-live",
                "Centering started · main-cam",
                Some("RA 10.5 · Dec 41.2 · tolerance 30″ · ≤ 3 attempts"),
            ),
            case(
                "centering_iteration",
                json!({"camera_id": "main-cam", "document_id": "d1", "residual_arcsec": 12.4, "action": "nudge", "solved_ra": 10.49, "solved_dec": 41.21}),
                "sev-ok",
                "Centering iteration · main-cam",
                Some("residual 12.4″ · nudge · RA 10.49 · Dec 41.21"),
            ),
            case(
                "centering_complete",
                json!({"camera_id": "main-cam", "final_error_arcsec": 8.2, "attempts": 2, "final_ra": 10.5, "final_dec": 41.2}),
                "sev-ok",
                "Centering complete · main-cam",
                Some("error 8.2″ · 2 attempts · RA 10.5 · Dec 41.2"),
            ),
            case(
                "centering_failed",
                json!({"error": "did not converge"}),
                "sev-bad",
                "Centering failed",
                Some("did not converge"),
            ),
            case(
                "plate_solve_started",
                json!({"document_id": "d1", "image_path": "/data/x.fits", "use_mount_hints": true}),
                "sev-live",
                "Plate solve started",
                Some("/data/x.fits"),
            ),
            case(
                "plate_solve_complete",
                json!({"ra_center": 10.68, "dec_center": 41.27, "pixel_scale_arcsec": 1.85, "rotation_deg": 12.3, "solver": "astap"}),
                "sev-ok",
                "Plate solve complete",
                Some("RA 10.68 · Dec 41.27 · 1.85″/px · rotation 12.3° · astap"),
            ),
            case(
                "plate_solve_failed",
                json!({"error": "no solution"}),
                "sev-bad",
                "Plate solve failed",
                Some("no solution"),
            ),
            case(
                "guide_started",
                json!({"settle_pixels": 1.5}),
                "sev-live",
                "Guiding started",
                None,
            ),
            case(
                "guide_settled",
                json!({"rms_ra_px": 0.42, "rms_dec_px": 0.51, "total_rms_px": 0.66, "sample_count": 120}),
                "sev-ok",
                "Guiding settled",
                Some("RMS 0.66 px · RA 0.42 · Dec 0.51 · n=120"),
            ),
            case(
                "guide_failed",
                json!({"error": "star lost"}),
                "sev-bad",
                "Guiding failed",
                Some("star lost"),
            ),
            case(
                "dither_started",
                json!({}),
                "sev-live",
                "Dither started",
                None,
            ),
            case(
                "dither_settled",
                json!({"total_rms_px": 0.5, "sample_count": 30}),
                "sev-ok",
                "Dither settled",
                Some("RMS 0.5 px · n=30"),
            ),
            case(
                "dither_failed",
                json!({"error": "did not settle"}),
                "sev-bad",
                "Dither failed",
                Some("did not settle"),
            ),
            case(
                "filter_switch",
                json!({"filter_wheel_id": "fw-1", "filter_name": "Ha"}),
                "sev-ok",
                "Filter switch · Ha",
                Some("fw-1"),
            ),
            case(
                "safety_changed",
                json!({"monitor": "cloudwatcher", "new_state": "safe"}),
                "sev-ok",
                "Safety: SAFE",
                Some("monitor cloudwatcher"),
            ),
            case(
                "safety_changed",
                json!({"monitor": "cloudwatcher", "new_state": "unsafe"}),
                "sev-bad",
                "Safety: UNSAFE",
                Some("monitor cloudwatcher"),
            ),
            case(
                "guide_stopped",
                json!({"reason": "safety stop"}),
                "sev-warn",
                "Guiding stopped",
                Some("safety stop"),
            ),
            case(
                "session_started",
                json!({"session_id": "s-1", "workflow_id": "deep_sky"}),
                "sev-live",
                "Session started",
                Some("workflow deep_sky · session s-1"),
            ),
            case(
                "session_stopped",
                json!({"reason": "end_of_session", "workflow_id": "deep_sky"}),
                "sev-ok",
                "Session stopped",
                Some("end_of_session · workflow deep_sky"),
            ),
            case(
                "document_persistence_failed",
                json!({"document_id": "d1", "file_path": "/data/x.fits", "error": "disk full"}),
                "sev-bad",
                "Document persistence failed",
                Some("/data/x.fits · disk full"),
            ),
            case("stream_error", json!({}), "sev-warn", "Stream error", None),
        ];
        for c in cases {
            let html = card(c.event, c.payload.clone());
            assert!(
                html.contains(&format!("feed-card {}", c.severity)),
                "{}: wrong severity (wanted {}): {html}",
                c.event,
                c.severity
            );
            assert!(
                html.contains(c.title),
                "{}: missing title {:?}: {html}",
                c.event,
                c.title
            );
            match c.detail {
                Some(detail) => assert!(
                    html.contains(detail),
                    "{}: missing detail {detail:?}: {html}",
                    c.event
                ),
                None => assert!(
                    !html.contains("evt-detail"),
                    "{}: unexpected detail: {html}",
                    c.event
                ),
            }
            assert!(
                html.contains(r#"<time class="mono">21:18:04</time>"#),
                "{}: missing sliced time: {html}",
                c.event
            );
        }
    }

    #[test]
    fn feed_card_unknown_event_degrades_to_generic_card() {
        // Unknown events must not vanish: humanized name + a canonical
        // (sorted-key) payload dump, neutral severity.
        let html = card("quantum_flux", json!({"b": 2, "a": 1}));
        assert!(html.contains("feed-card sev-ok"), "{html}");
        assert!(html.contains("Quantum flux"), "{html}");
        // Sorted keys prove determinism regardless of `preserve_order`.
        assert!(html.contains("{&quot;a&quot;:1,&quot;b&quot;:2}"), "{html}");
    }

    #[test]
    fn feed_card_appends_humanized_elapsed_to_the_detail() {
        let mut env = envelope("park_complete", json!({}));
        env.elapsed_ms = Some(124_000);
        let html = feed_card(&env).into_string();
        assert!(html.contains("evt-detail"), "{html}");
        assert!(html.contains("2m 4s"), "{html}");
    }

    #[test]
    fn feed_card_renders_missing_payload_fields_as_omissions() {
        // Rendering is total: a started event with an empty payload still
        // renders a card (title + time, no detail).
        let html = card("slew_started", json!({}));
        assert!(html.contains("Slew started"), "{html}");
        assert!(!html.contains("evt-detail"), "{html}");
        // …and a payload of the wrong shape is ignored, not an error.
        let html = card("slew_started", json!({"ra": {"nested": true}}));
        assert!(!html.contains("evt-detail"), "{html}");
    }

    #[test]
    fn humanize_ms_formats_all_magnitudes() {
        assert_eq!(humanize_ms(0), "0s");
        assert_eq!(humanize_ms(400), "400ms");
        assert_eq!(humanize_ms(5_000), "5s");
        assert_eq!(humanize_ms(65_000), "1m 5s");
        assert_eq!(humanize_ms(124_000), "2m 4s");
        assert_eq!(humanize_ms(3_723_000), "1h 2m 3s");
    }

    #[test]
    fn short_time_slices_hh_mm_ss_with_raw_fallback() {
        assert_eq!(short_time("2026-07-10T21:18:04Z"), "21:18:04");
        // Millisecond timestamps still slice the HH:MM:SS window.
        assert_eq!(short_time("2026-07-10T21:18:04.123Z"), "21:18:04");
        // Anything too short falls back to the raw string.
        assert_eq!(short_time("soon"), "soon");
        assert_eq!(short_time(""), "");
    }

    #[test]
    fn num_display_trims_floats_and_keeps_integers() {
        let n = |v: Value| match v {
            Value::Number(n) => num_display(&n),
            _ => unreachable!(),
        };
        assert_eq!(n(json!(5210)), "5210");
        assert_eq!(n(json!(0.42)), "0.42");
        assert_eq!(n(json!(10.684_583_333)), "10.6846");
        assert_eq!(n(json!(2.0)), "2");
        assert_eq!(n(json!(-2.25)), "-2.25");
    }

    // --- the gap divider + stream_error card -----------------------------------

    #[test]
    fn gap_divider_renders_the_eviction_shape() {
        let html = gap_divider(
            &json!({"event": "stream_gap", "requested_after": 5, "oldest_available": 9}),
        )
        .into_string();
        assert!(html.contains("feed-gap"), "{html}");
        assert!(html.contains("requested after 5"), "{html}");
        assert!(html.contains("oldest available 9"), "{html}");
    }

    #[test]
    fn gap_divider_renders_the_lagged_shape() {
        let html = gap_divider(&json!({"event": "stream_gap", "lagged": 3})).into_string();
        assert!(html.contains("feed-gap"), "{html}");
        assert!(html.contains("lagged by 3"), "{html}");
    }

    #[test]
    fn gap_divider_renders_a_bare_gap() {
        let html = gap_divider(&json!({"event": "stream_gap"})).into_string();
        assert!(html.contains("feed-gap"), "{html}");
        assert!(html.contains("events were missed"), "{html}");
    }

    #[test]
    fn stream_error_card_is_a_warn_card_with_optional_detail() {
        let html = stream_error_card("").into_string();
        assert!(html.contains("feed-card sev-warn"), "{html}");
        assert!(html.contains("Stream error"), "{html}");
        assert!(!html.contains("evt-detail"), "{html}");

        let html = stream_error_card("serialization failed").into_string();
        assert!(html.contains("serialization failed"), "{html}");
    }

    // --- slot updates -----------------------------------------------------------

    /// One slot-mapping case: event name, payload, expected (slot, substring).
    type SlotCase = (&'static str, Value, Vec<(&'static str, &'static str)>);

    /// Which slots fire for which events, and their inner text.
    #[test]
    fn slot_updates_maps_events_to_slots() {
        let cases: Vec<SlotCase> = vec![
            ("slew_started", json!({}), vec![("operation", "Slewing…")]),
            (
                "exposure_started",
                json!({"camera_id": "main-cam", "duration": "5s"}),
                vec![("operation", "Exposing · main-cam · 5s")],
            ),
            (
                "move_focuser_started",
                json!({"focuser_id": "foc-1", "position": 5210}),
                vec![("operation", "Moving focuser · foc-1 → 5210")],
            ),
            (
                "focus_started",
                json!({"camera_id": "main-cam"}),
                vec![("operation", "Focusing · main-cam")],
            ),
            (
                "plate_solve_started",
                json!({}),
                vec![("operation", "Plate solving…")],
            ),
            (
                "slew_complete",
                json!({}),
                vec![("operation", "Slew complete")],
            ),
            (
                "exposure_failed",
                json!({"error": "x"}),
                vec![("operation", "Exposure failed")],
            ),
            (
                "sync_mount_complete",
                json!({}),
                vec![("operation", "Mount sync complete")],
            ),
            (
                "guide_settled",
                json!({"total_rms_px": 0.42, "sample_count": 120}),
                vec![
                    ("guide", "RMS 0.42 px · n=120"),
                    ("operation", "Guiding settled"),
                ],
            ),
            (
                "dither_settled",
                json!({"total_rms_px": 0.5, "sample_count": 30}),
                vec![
                    ("guide", "RMS 0.5 px · n=30"),
                    ("operation", "Dither settled"),
                ],
            ),
            (
                "session_started",
                json!({}),
                vec![("session", "session active")],
            ),
            (
                "session_stopped",
                json!({}),
                vec![("session", "session stopped")],
            ),
            (
                "safety_changed",
                json!({"new_state": "unsafe"}),
                vec![("session", "UNSAFE")],
            ),
            (
                "safety_changed",
                json!({"new_state": "safe"}),
                vec![("session", "SAFE")],
            ),
            // Point events leave every slot alone.
            ("filter_switch", json!({"filter_name": "Ha"}), vec![]),
            ("centering_iteration", json!({}), vec![]),
            ("guide_stopped", json!({"reason": "safety"}), vec![]),
            ("document_persistence_failed", json!({}), vec![]),
            // Unknown started/terminal events still drive the operation slot.
            (
                "custom_thing_started",
                json!({}),
                vec![("operation", "Custom thing started")],
            ),
            (
                "custom_thing_complete",
                json!({}),
                vec![("operation", "Custom thing complete")],
            ),
        ];
        for (event, payload, expected) in cases {
            let rendered: Vec<(&'static str, String)> = slot_updates(&envelope(event, payload))
                .into_iter()
                .map(|(name, markup)| (name, markup.into_string()))
                .collect();
            assert_eq!(
                rendered.len(),
                expected.len(),
                "{event}: wrong slot set: {rendered:?}"
            );
            for ((name, html), (want_name, want_substr)) in rendered.iter().zip(&expected) {
                assert_eq!(name, want_name, "{event}: wrong slot name");
                assert!(
                    html.contains(want_substr),
                    "{event}: slot {name}: {html:?} missing {want_substr:?}"
                );
            }
        }
    }

    #[test]
    fn safety_chips_carry_severity_classes() {
        let unsafe_chip = safety_chip(&json!({"new_state": "unsafe"})).into_string();
        assert!(unsafe_chip.contains("chip bad"), "{unsafe_chip}");
        let safe_chip = safety_chip(&json!({"new_state": "safe"})).into_string();
        assert!(safe_chip.contains("chip ok"), "{safe_chip}");
        let odd_chip = safety_chip(&json!({})).into_string();
        assert!(odd_chip.contains("chip warn"), "{odd_chip}");
    }

    #[test]
    fn session_chips_map_states_to_classes() {
        assert!(session_state_chip("active")
            .into_string()
            .contains("chip live"));
        assert!(session_state_chip("idle")
            .into_string()
            .contains("chip muted"));
        assert!(session_state_chip("interrupted")
            .into_string()
            .contains("chip warn"));
        assert!(session_state_chip("unknown")
            .into_string()
            .contains("chip muted"));
    }

    #[test]
    fn unreachable_slot_names_the_condition() {
        assert!(unreachable_slot()
            .into_string()
            .contains("rp unreachable — retrying…"));
    }

    // --- the page + equipment fragment ------------------------------------------

    /// A `ConfigClient` the stream surfaces never call (state plumbing only).
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

    fn rp_state(api: MockRpApi) -> AppState {
        AppState::with_rp_parts(
            Arc::new(UnusedConfigClient),
            Arc::new(api),
            Arc::new(crate::probe::MockProbeHttp::new()),
        )
    }

    fn equipment_fixture() -> EquipmentStatus {
        EquipmentStatus {
            cameras: vec![DeviceStatus {
                id: "main-cam".to_string(),
                connected: true,
            }],
            cover_calibrators: vec![DeviceStatus {
                id: "flat".to_string(),
                connected: false,
            }],
            mount: Some(MountStatus { connected: true }),
            ..Default::default()
        }
    }

    fn down() -> ConfigClientError {
        ConfigClientError::Transport("connection refused".to_string())
    }

    #[tokio::test]
    async fn page_renders_the_sse_wiring_and_initial_state() {
        let mut api = MockRpApi::new();
        api.expect_session_status()
            .returning(|| Box::pin(async { Ok("active".to_string()) }));
        api.expect_equipment_status()
            .returning(|| Box::pin(async { Ok(equipment_fixture()) }));
        let html = page(State(rp_state(api))).await.into_string();

        // The SSE extension script + the one sse-connect wrapper.
        assert!(html.contains(r#"src="/assets/htmx-ext-sse.js""#), "{html}");
        assert!(html.contains(r#"hx-ext="sse""#), "{html}");
        assert!(html.contains(r#"sse-connect="/stream/events""#), "{html}");
        // The fold: checkbox + label strip.
        assert!(html.contains(r#"id="fold-state""#), "{html}");
        assert!(html.contains(r#"class="fold-strip""#), "{html}");
        // The three slots with their swap names + initial content.
        assert!(html.contains(r#"id="slot-operation""#), "{html}");
        assert!(html.contains(r#"sse-swap="operation""#), "{html}");
        assert!(html.contains("Idle"), "{html}");
        assert!(html.contains(r#"id="slot-guide""#), "{html}");
        assert!(html.contains(r#"sse-swap="guide""#), "{html}");
        assert!(html.contains("RMS —"), "{html}");
        assert!(html.contains(r#"id="slot-session""#), "{html}");
        assert!(html.contains(r#"sse-swap="session""#), "{html}");
        assert!(html.contains("session active"), "{html}");
        // The LED panel with the poll wiring and the roster entries.
        assert!(html.contains(r#"id="equipment-leds""#), "{html}");
        assert!(html.contains(r#"hx-get="/stream/equipment""#), "{html}");
        assert!(html.contains(r#"hx-trigger="every 10s""#), "{html}");
        assert!(html.contains("main-cam"), "{html}");
        assert!(html.contains("mount"), "{html}");
        // The feed region, initially empty, prepend-swapped.
        assert!(html.contains(r#"id="feed""#), "{html}");
        assert!(html.contains(r#"sse-swap="feed""#), "{html}");
        assert!(html.contains(r#"hx-swap="afterbegin""#), "{html}");
    }

    #[tokio::test]
    async fn page_renders_unreachable_state_when_rp_is_down() {
        let mut api = MockRpApi::new();
        api.expect_session_status()
            .returning(|| Box::pin(async { Err(down()) }));
        api.expect_equipment_status()
            .returning(|| Box::pin(async { Err(down()) }));
        let html = page(State(rp_state(api))).await.into_string();

        // The page still renders (shell + SSE wiring)…
        assert!(html.contains(r#"sse-connect="/stream/events""#), "{html}");
        // …with the operation slot in its unreachable state, the session chip
        // unknown, and the LED panel note.
        assert!(html.contains("rp unreachable — retrying…"), "{html}");
        assert!(html.contains("session unknown"), "{html}");
        assert!(
            html.contains("rp unreachable — equipment state unknown"),
            "{html}"
        );
    }

    #[tokio::test]
    async fn page_without_rp_target_renders_the_shared_card() {
        let state = AppState::with_client("dsd-fp2", Arc::new(UnusedConfigClient));
        let html = page(State(state)).await.into_string();
        assert!(html.contains("No rp orchestrator is configured"), "{html}");
        assert!(html.contains("the activity stream"), "{html}");
    }

    #[tokio::test]
    async fn equipment_fragment_renders_leds_per_device() {
        let mut api = MockRpApi::new();
        api.expect_equipment_status()
            .returning(|| Box::pin(async { Ok(equipment_fixture()) }));
        let html = equipment_fragment(State(rp_state(api))).await.into_string();
        assert!(html.contains(r#"id="equipment-leds""#), "{html}");
        // Connected camera → ok LED; disconnected cover calibrator → bad LED.
        assert!(html.contains(r#"<span class="led ok"></span><span class="kind">camera</span><span class="dev-id">main-cam</span>"#), "{html}");
        assert!(html.contains(r#"<span class="led bad"></span><span class="kind">cover calibrator</span><span class="dev-id">flat</span>"#), "{html}");
        // The singular mount is labelled "mount" with no id.
        assert!(
            html.contains(r#"<span class="led ok"></span><span class="kind">mount</span>"#),
            "{html}"
        );
    }

    #[tokio::test]
    async fn equipment_fragment_degrades_to_a_note_when_rp_is_down() {
        let mut api = MockRpApi::new();
        api.expect_equipment_status()
            .returning(|| Box::pin(async { Err(down()) }));
        let html = equipment_fragment(State(rp_state(api))).await.into_string();
        // The fragment keeps its own poll wiring so it self-heals.
        assert!(html.contains(r#"hx-get="/stream/equipment""#), "{html}");
        assert!(
            html.contains("rp unreachable — equipment state unknown"),
            "{html}"
        );
    }

    #[tokio::test]
    async fn equipment_fragment_notes_an_empty_roster() {
        let mut api = MockRpApi::new();
        api.expect_equipment_status()
            .returning(|| Box::pin(async { Ok(EquipmentStatus::default()) }));
        let html = equipment_fragment(State(rp_state(api))).await.into_string();
        assert!(html.contains("No equipment configured."), "{html}");
    }
}

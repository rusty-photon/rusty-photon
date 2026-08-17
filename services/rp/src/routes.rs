use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::config_actions::RpConfigDriver;
use crate::equipment::EquipmentRegistry;
use crate::events::{EventEnvelope, Subscription};
use crate::mcp::McpHandler;
use crate::persistence::{CachedPixels, ImageCache};
use crate::session::SessionManager;

/// ASCOM `TransmissionElementType` code for `u16` payloads.
const TRANSMISSION_U16: i32 = 8;
/// ASCOM `TransmissionElementType` code for `i32` payloads.
const TRANSMISSION_I32: i32 = 2;
/// ASCOM `ImageElementType` is always `Int32` (the logical type required by
/// the Alpaca API). The transmission type may differ.
const IMAGE_ELEMENT_I32: i32 = 2;
const IMAGEBYTES_HEADER_LEN: usize = 44;

#[derive(Clone)]
pub struct AppState {
    pub equipment: Arc<EquipmentRegistry>,
    pub mcp: McpHandler,
    pub session: Arc<SessionManager>,
    pub image_cache: ImageCache,
    /// Cancelled when rp begins graceful shutdown; the SSE handler selects on
    /// it to end in-flight `/api/events/subscribe` streams. See
    /// `BoundServer::start`.
    pub sse_shutdown: CancellationToken,
    /// `false` while safety conditions are unsafe — the `/mcp` gate below
    /// rejects every request with 503 (rp.md § Safety). Written by the
    /// safety enforcer.
    pub safety_ok: Arc<AtomicBool>,
    /// The MCP transport's session registry, shared with the safety
    /// enforcer so an unsafe transition can terminate every open session.
    pub mcp_sessions: Arc<LocalSessionManager>,
    /// The effective running configuration (rp has no config-overriding CLI
    /// flags, so this is exactly the file loaded at startup). Served by
    /// `GET /api/config` (secrets redacted) and diffed against by
    /// `PUT /api/config` to compute `restart_required[]`.
    pub config: Arc<crate::config::Config>,
    /// The resolved config-file path `PUT /api/config` persists to.
    pub config_path: Arc<std::path::PathBuf>,
}

/// Build rp's router. `mcp_extra_allowed_hosts` are the `Host`
/// authorities the MCP transport accepts on top of rmcp's loopback
/// defaults — the names rp advertises itself as (rp.md § MCP Server).
pub fn build_router(state: AppState, mcp_extra_allowed_hosts: Vec<String>) -> Router {
    let mcp_handler = state.mcp.clone();
    let mut mcp_config = StreamableHttpServerConfig::default();
    mcp_config.json_response = true;
    // rmcp's DNS-rebinding protection answers 403 to any `Host` outside
    // this list, whose defaults cover loopback only — which would reject
    // the hostname URL rp advertises for a wildcard bind. Extend it, never
    // replace it: an empty list switches the protection off entirely.
    mcp_config.allowed_hosts.extend(mcp_extra_allowed_hosts);
    let mcp_service = StreamableHttpService::new(
        move || Ok(mcp_handler.clone()),
        state.mcp_sessions.clone(),
        mcp_config,
    );

    // The safety gate (rp.md § Safety): while conditions are unsafe every
    // MCP request — a new session or an in-flight workflow's next call —
    // is rejected with 503, surfacing to the orchestrator as a terminated
    // session instead of quietly driving hardware under unsafe skies.
    let safety_ok = state.safety_ok.clone();
    let gated_mcp =
        Router::new()
            .nest_service("/mcp", mcp_service)
            .layer(axum::middleware::from_fn(
                move |request: axum::extract::Request, next: axum::middleware::Next| {
                    let safety_ok = safety_ok.clone();
                    async move {
                        if safety_ok.load(Ordering::SeqCst) {
                            next.run(request).await
                        } else {
                            (
                                StatusCode::SERVICE_UNAVAILABLE,
                                "safety: conditions are unsafe; the workflow was cancelled",
                            )
                                .into_response()
                        }
                    }
                },
            ));

    Router::new()
        .route("/health", get(health))
        .route("/api/equipment", get(get_equipment))
        // Plain-REST config endpoints (no Alpaca envelope — rp is not an
        // ASCOM device). Deliberately outside the /mcp safety gate: editing
        // config must stay possible under unsafe skies. Router-wide auth/TLS
        // layers (applied in lib.rs) cover them like every other route.
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/config/schema", get(get_config_schema))
        .merge(gated_mcp)
        .route("/api/session/start", post(session_start))
        .route("/api/session/stop", post(session_stop))
        .route("/api/session/status", get(session_status))
        .route(
            "/api/plugins/{workflow_id}/complete",
            post(workflow_complete),
        )
        .route("/api/documents/{document_id}", get(get_document))
        .route("/api/images/{document_id}", get(get_image_metadata))
        .route("/api/images/{document_id}/pixels", get(get_image_pixels))
        .route("/api/events/subscribe", get(subscribe_events))
        .with_state(state)
}

async fn health() -> &'static str {
    "Hello World, I am healthy!"
}

async fn get_equipment(State(state): State<AppState>) -> Json<Value> {
    let status = state.equipment.status();
    Json(serde_json::to_value(status).unwrap_or_default())
}

/// `GET /api/config` — the effective running config with secrets redacted to
/// the `********` sentinel, plus the CLI-override-pinned paths (always empty:
/// rp has no config-overriding flags). Body is a bare
/// [`rusty_photon_config::actions::ConfigGetResponse`].
async fn get_config(State(state): State<AppState>) -> Response {
    debug!("GET /api/config");
    match rusty_photon_config::actions::config_get::<RpConfigDriver>(&state.config, &()) {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => {
            debug!(error = %e, "failed to serialize effective config");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to serialize config: {e}"),
            )
                .into_response()
        }
    }
}

/// `GET /api/config/schema` — a JSON Schema for rp's config plus the
/// editability tiers: `locked_fields` is empty and `read_only_fields` is
/// `["server.port"]` (a rebind the UI could not follow). Body is a bare
/// [`rusty_photon_config::actions::ConfigSchemaResponse`].
async fn get_config_schema() -> Json<rusty_photon_config::actions::ConfigSchemaResponse> {
    debug!("GET /api/config/schema");
    Json(rusty_photon_config::actions::config_schema::<RpConfigDriver>())
}

/// `PUT /api/config` — validate and persist a full submitted config JSON.
///
/// rp has no in-process reload (`main.rs` runs `ServiceRunner::run`), so
/// every changed field is classified `restart_required` and `status` stays
/// `"ok"` — the persisted file takes effect on the next rp start. Validation
/// failure is HTTP 200 `status:"invalid"` with `errors[]`, file untouched. A
/// malformed JSON body is 400 with a plain-text message; internal read /
/// persist failures are 500.
async fn put_config(State(state): State<AppState>, body: String) -> Response {
    use rusty_photon_config::actions::{config_apply, ApplyError};

    debug!("PUT /api/config");
    match config_apply::<RpConfigDriver>(&state.config_path, &(), &state.config, &body) {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(err @ ApplyError::Parse(_)) => {
            debug!(error = %err, "config apply rejected: malformed JSON body");
            (StatusCode::BAD_REQUEST, err.to_string()).into_response()
        }
        Err(err) => {
            debug!(error = %err, "config apply failed");
            (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response()
        }
    }
}

/// Interval between SSE `:keep-alive` comment lines, to keep middleboxes from
/// closing an idle stream.
const SSE_KEEP_ALIVE: Duration = Duration::from_secs(15);

/// Query parameters for [`subscribe_events`]. `last_event_id` is the explicit,
/// testable form of the `Last-Event-ID` reconnect cursor (the header takes
/// precedence — see [`parse_last_event_id`]).
#[derive(serde::Deserialize)]
struct SubscribeParams {
    last_event_id: Option<u64>,
}

/// `GET /api/events/subscribe` — stream every emitted event as Server-Sent
/// Events. Each frame's SSE `id` is the envelope's `event_seq`, the SSE
/// `event` is the event type, and `data` is the full [`EventEnvelope`] JSON.
///
/// On reconnect the client sends its last seen `event_seq` (the `Last-Event-ID`
/// header, or `?last_event_id=`); buffered events after it are replayed
/// oldest-first before the live tail. If history was evicted past the cursor a
/// leading `stream_gap` event signals the loss. A consumer that lags the live
/// channel is sent a final `stream_gap` and disconnected so it reconnects and
/// replays from history. The stream ends when the client goes away or rp shuts
/// down (cancelling `state.sse_shutdown`).
async fn subscribe_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SubscribeParams>,
) -> impl IntoResponse {
    let last_seq = parse_last_event_id(&headers, &params);
    let subscription = state.mcp.event_bus.subscribe_with_history(last_seq);
    let gap = detect_gap(last_seq, subscription.oldest_retained_seq);
    let Subscription {
        replay,
        mut receiver,
        ..
    } = subscription;
    let shutdown = state.sse_shutdown;

    let stream = async_stream::stream! {
        // Lead with a stream_gap if the reconnect cursor predates retained
        // history, so the client knows it lost events.
        if let Some(gap) = gap {
            yield Ok::<Event, std::convert::Infallible>(gap_event(&gap));
        }
        // Replay buffered events after the cursor, oldest first.
        for envelope in replay {
            yield Ok(envelope_event(&envelope));
        }
        // Then the live tail until the client disconnects or rp shuts down.
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                received = receiver.recv() => match received {
                    Ok(envelope) => yield Ok(envelope_event(&envelope)),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        debug!(missed, "SSE consumer lagged the broadcast channel; closing stream for reconnect");
                        yield Ok(lag_gap_event(missed));
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::new().interval(SSE_KEEP_ALIVE))
}

/// The `Last-Event-ID` reconnect cursor: the header (set automatically by the
/// browser `EventSource` API and by Sentinel) wins, with `?last_event_id=` as
/// an explicit fallback. `None` means "start from the live tail".
fn parse_last_event_id(headers: &HeaderMap, params: &SubscribeParams) -> Option<u64> {
    headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .or(params.last_event_id)
}

/// The events a reconnecting client missed that history can no longer replay.
struct GapInfo {
    /// The client's reconnect cursor (`Last-Event-ID`).
    requested_after: u64,
    /// The oldest `event_seq` still retained; everything in
    /// `(requested_after, oldest_available)` was evicted.
    oldest_available: u64,
}

/// Decide whether a reconnecting client missed events that the history ring no
/// longer holds. A gap exists when the client's cursor is below the oldest
/// retained seq minus one — i.e. the very next event it expected was already
/// evicted.
const fn detect_gap(
    requested_last_seq: Option<u64>,
    oldest_retained_seq: Option<u64>,
) -> Option<GapInfo> {
    match (requested_last_seq, oldest_retained_seq) {
        // `saturating_add` guards against a client sending a near-`u64::MAX`
        // `Last-Event-ID` (which would otherwise overflow-panic in debug).
        (Some(after), Some(oldest)) if oldest > after.saturating_add(1) => Some(GapInfo {
            requested_after: after,
            oldest_available: oldest,
        }),
        _ => None,
    }
}

/// Map an event envelope to an SSE frame: `id` = `event_seq`, `event` = type,
/// `data` = the envelope JSON.
fn envelope_event(envelope: &EventEnvelope) -> Event {
    Event::default().json_data(envelope).map_or_else(
        // EventEnvelope always serializes; degrade without panicking just in case.
        |_| Event::default().event("stream_error"),
        |event| {
            event
                .id(envelope.event_seq.to_string())
                .event(envelope.event.clone())
        },
    )
}

/// A `stream_gap` frame telling a reconnecting client that events between its
/// cursor and the oldest retained event were evicted. Carries no `id` (it is
/// informational and must not become a reconnect cursor itself).
fn gap_event(gap: &GapInfo) -> Event {
    let data = serde_json::json!({
        "event": "stream_gap",
        "requested_after": gap.requested_after,
        "oldest_available": gap.oldest_available,
    });
    gap_frame(&data)
}

/// A `stream_gap` frame emitted when a live consumer lagged the broadcast
/// channel by `missed` events and is about to be disconnected.
fn lag_gap_event(missed: u64) -> Event {
    let data = serde_json::json!({ "event": "stream_gap", "lagged": missed });
    gap_frame(&data)
}

fn gap_frame(data: &Value) -> Event {
    Event::default()
        .json_data(data)
        .unwrap_or_default()
        .event("stream_gap")
}

async fn session_start(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    match state.session.start().await {
        Ok(body) => (StatusCode::OK, Json(body)),
        Err(e) => (StatusCode::CONFLICT, Json(serde_json::json!({"error": e}))),
    }
}

async fn session_stop(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    match state.session.stop().await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "stopped"})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        ),
    }
}

async fn session_status(State(state): State<AppState>) -> Json<Value> {
    let status = state.session.status().await;
    Json(serde_json::json!({"status": status}))
}

async fn workflow_complete(
    State(state): State<AppState>,
    Path(workflow_id): Path<String>,
) -> StatusCode {
    debug!(workflow_id = %workflow_id, "received workflow completion");
    state.session.workflow_complete(&workflow_id).await;
    StatusCode::OK
}

async fn get_document(
    State(state): State<AppState>,
    Path(document_id): Path<String>,
) -> (StatusCode, Json<Value>) {
    state
        .image_cache
        .resolve_document(&document_id)
        .await
        .map_or_else(
            || {
                (
                    StatusCode::NOT_FOUND,
                    Json(
                        serde_json::json!({"error": format!("document not found: {}", document_id)}),
                    ),
                )
            },
            |doc| match serde_json::to_value(&doc) {
                Ok(v) => (StatusCode::OK, Json(v)),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                ),
            },
        )
}

async fn get_image_metadata(
    State(state): State<AppState>,
    Path(document_id): Path<String>,
) -> (StatusCode, Json<Value>) {
    // `resolve` and `resolve_document` have asymmetric semantics: `resolve`
    // declines when the sidecar's `max_adu` is null because the cache
    // can't pick a `CachedPixels` variant, while `resolve_document` still
    // returns the document so callers can read the FITS via `file_path`.
    // Metadata must be reachable for any document on disk, so try the
    // pixel-bearing path first (gives us `bitpix` and `in_cache=true`) and
    // fall back to a doc-only resolve when the cache cannot rehydrate
    // pixels.
    let cached = state.image_cache.resolve(&document_id).await;
    let in_cache = cached.is_some();
    let bitpix = cached.as_ref().map(|c| match &c.pixels {
        CachedPixels::U16(_) => 16,
        CachedPixels::I32(_) => 32,
    });
    let doc = match cached {
        Some(c) => c.document.read().await.clone(),
        None => match state.image_cache.resolve_document(&document_id).await {
            Some(d) => d,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "error": format!("document not found: {}", document_id)
                    })),
                );
            }
        },
    };
    let body = serde_json::json!({
        "document_id": document_id,
        "width": doc.width,
        "height": doc.height,
        "bitpix": bitpix,
        "fits_path": doc.file_path,
        "in_cache": in_cache,
        "document_url": format!("/api/documents/{}", document_id),
    });
    (StatusCode::OK, Json(body))
}

async fn get_image_pixels(
    State(state): State<AppState>,
    Path(document_id): Path<String>,
) -> Response {
    let Some(cached) = state.image_cache.resolve(&document_id).await else {
        return not_found(&format!("document not found: {document_id}"));
    };
    let (width, height) = (cached.width, cached.height);
    let body = match &cached.pixels {
        CachedPixels::U16(arr) => imagebytes(width, height, TRANSMISSION_U16, |buf| {
            buf.reserve(arr.len().saturating_mul(2));
            for &v in arr {
                buf.extend_from_slice(&v.to_le_bytes());
            }
        }),
        CachedPixels::I32(arr) => imagebytes(width, height, TRANSMISSION_I32, |buf| {
            buf.reserve(arr.len().saturating_mul(4));
            for &v in arr {
                buf.extend_from_slice(&v.to_le_bytes());
            }
        }),
    };
    imagebytes_response(body)
}

/// Build an Alpaca `ImageBytes` payload (44-byte header + raw little-endian
/// pixel bytes). The header layout mirrors `ascom-alpaca`'s
/// `ImageBytesMetadata` (which is `pub(crate)` upstream, so we replicate
/// it here). All fields are i32 little-endian.
fn imagebytes(
    width: u32,
    height: u32,
    transmission_element_type: i32,
    write_pixels: impl FnOnce(&mut Vec<u8>),
) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::with_capacity(IMAGEBYTES_HEADER_LEN);
    let header_fields: [i32; 11] = [
        1,                                                        // metadata_version
        0,                                                        // error_number
        0,                                                        // client_transaction_id
        0,                                                        // server_transaction_id
        i32::try_from(IMAGEBYTES_HEADER_LEN).unwrap_or(i32::MAX), // data_start
        IMAGE_ELEMENT_I32,         // image_element_type (logical, always Int32)
        transmission_element_type, // transmission_element_type
        2,                         // rank
        width.cast_signed(),       // dimension_1
        height.cast_signed(),      // dimension_2
        0,                         // dimension_3
    ];
    for f in &header_fields {
        buf.extend_from_slice(&f.to_le_bytes());
    }
    debug_assert_eq!(buf.len(), IMAGEBYTES_HEADER_LEN);
    write_pixels(&mut buf);
    buf
}

fn imagebytes_response(body: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/imagebytes")],
        body,
    )
        .into_response()
}

fn not_found(msg: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": msg})),
    )
        .into_response()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
// Slicing a literal UUID down to its 8-char disk key. There is no
// `allow-string-slice-in-tests` knob, so the exemption is scoped here.
#[allow(clippy::string_slice)]
mod tests {
    use super::*;
    use crate::events::EventBus;
    use crate::persistence::{CachedImage, ExposureDocument};
    use std::path::PathBuf;

    fn scaffold_config() -> crate::config::Config {
        serde_json::from_value(crate::config::default_scaffold()).unwrap()
    }

    fn test_app_state(image_cache: ImageCache) -> AppState {
        test_app_state_with_config(
            image_cache,
            scaffold_config(),
            PathBuf::from("/nonexistent/rp-test-config.json"),
        )
    }

    fn test_app_state_with_config(
        image_cache: ImageCache,
        config: crate::config::Config,
        config_path: PathBuf,
    ) -> AppState {
        let event_bus = Arc::new(EventBus::from_config(&[], None).unwrap());
        let equipment = Arc::new(crate::equipment::EquipmentRegistry {
            safety_monitors: vec![],
            cameras: vec![],
            filter_wheels: vec![],
            cover_calibrators: vec![],
            focusers: vec![],
            mount: None,
            ..Default::default()
        });
        let session =
            Arc::new(crate::session::SessionManager::new(event_bus.clone(), &[], None).unwrap());
        let mcp = McpHandler::new(
            equipment.clone(),
            event_bus,
            crate::session::SessionConfig {
                data_directory: "/tmp".to_string(),
            },
            image_cache.clone(),
            None,
        );
        AppState {
            equipment,
            mcp,
            session,
            image_cache,
            sse_shutdown: CancellationToken::new(),
            safety_ok: Arc::new(AtomicBool::new(true)),
            mcp_sessions: Arc::new(LocalSessionManager::default()),
            config: Arc::new(config),
            config_path: Arc::new(config_path),
        }
    }

    #[tokio::test]
    async fn mcp_gate_rejects_with_503_while_unsafe_and_lifts_after() {
        let state = test_app_state(ImageCache::new(64, 4, std::path::PathBuf::from("/tmp")));
        let safety_ok = state.safety_ok.clone();
        let app = build_router(state, vec![]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });

        let client = reqwest::Client::new();
        let probe = || {
            client
                .post(format!("http://{addr}/mcp"))
                .header("accept", "application/json, text/event-stream")
                .json(&serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-03-26",
                        "capabilities": {},
                        "clientInfo": {"name": "gate-test", "version": "0"}
                    }
                }))
                .send()
        };

        safety_ok.store(false, Ordering::SeqCst);
        let gated = probe().await.unwrap();
        assert_eq!(gated.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);

        safety_ok.store(true, Ordering::SeqCst);
        let open = probe().await.unwrap();
        assert_ne!(
            open.status(),
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "the gate must lift once conditions are safe again"
        );

        // Only /mcp is gated — the REST API keeps answering while unsafe.
        safety_ok.store(false, Ordering::SeqCst);
        let health = client
            .get(format!("http://{addr}/health"))
            .send()
            .await
            .unwrap();
        assert!(health.status().is_success());
        // The config endpoints in particular must stay reachable under
        // unsafe skies (editing config is how an operator recovers).
        let config = client
            .get(format!("http://{addr}/api/config"))
            .send()
            .await
            .unwrap();
        assert!(config.status().is_success());
        let schema = client
            .get(format!("http://{addr}/api/config/schema"))
            .send()
            .await
            .unwrap();
        assert!(schema.status().is_success());
        let _ = tx.send(());
    }

    /// The extra allowed hosts must reach rmcp's DNS-rebinding check:
    /// an advertised name is served, everything else still gets 403.
    /// The loopback defaults survive the extension (the request that
    /// carries no explicit `Host` override addresses `127.0.0.1`).
    #[tokio::test]
    async fn mcp_serves_an_extra_allowed_host_and_rejects_an_unknown_one() {
        let state = test_app_state(ImageCache::new(64, 4, std::path::PathBuf::from("/tmp")));
        let app = build_router(state, vec!["observatory.example".to_string()]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });

        let client = reqwest::Client::new();
        let initialize = |host: Option<&str>| {
            let mut request = client
                .post(format!("http://{addr}/mcp"))
                .header("accept", "application/json, text/event-stream");
            if let Some(host) = host {
                request = request.header(header::HOST, host);
            }
            request
                .json(&serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-03-26",
                        "capabilities": {},
                        "clientInfo": {"name": "host-test", "version": "0"}
                    }
                }))
                .send()
        };

        let advertised = initialize(Some("observatory.example")).await.unwrap();
        assert_eq!(advertised.status(), reqwest::StatusCode::OK);

        let loopback = initialize(None).await.unwrap();
        assert_eq!(
            loopback.status(),
            reqwest::StatusCode::OK,
            "extending the allowlist must not drop the loopback defaults"
        );

        let unknown = initialize(Some("attacker.example")).await.unwrap();
        assert_eq!(unknown.status(), reqwest::StatusCode::FORBIDDEN);
        let _ = tx.send(());
    }

    /// A wildcard bind contributes interface addresses as bare strings,
    /// while a client addressing an IPv6 listener sends the bracketed
    /// `[addr]:port` form. Pins that the two match — the entry format
    /// `additional_allowed_hosts` emits is the one rmcp accepts.
    #[tokio::test]
    async fn mcp_serves_a_bare_ipv6_allowed_host_addressed_in_bracketed_form() {
        let state = test_app_state(ImageCache::new(64, 4, std::path::PathBuf::from("/tmp")));
        let app = build_router(state, vec!["2001:db8::1".to_string()]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });

        let response = reqwest::Client::new()
            .post(format!("http://{addr}/mcp"))
            .header("accept", "application/json, text/event-stream")
            .header(header::HOST, "[2001:db8::1]:11115")
            .json(&serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {"name": "host-test", "version": "0"}
                }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let _ = tx.send(());
    }

    fn cached_u16(arr: ndarray::Array2<u16>) -> CachedImage {
        let (w, h) = arr.dim();
        CachedImage::new(
            CachedPixels::U16(arr),
            w as u32,
            h as u32,
            PathBuf::from("/tmp/fake.fits"),
            65535,
            doc_at("/tmp/fake.fits"),
        )
    }

    fn cached_i32(arr: ndarray::Array2<i32>) -> CachedImage {
        let (w, h) = arr.dim();
        CachedImage::new(
            CachedPixels::I32(arr),
            w as u32,
            h as u32,
            PathBuf::from("/tmp/fake.fits"),
            1 << 20,
            doc_at("/tmp/fake.fits"),
        )
    }

    fn doc_at(file_path: &str) -> ExposureDocument {
        ExposureDocument {
            target: None,
            frame_type: None,
            id: "doc-1".to_string(),
            captured_at: "2026-04-30T00:00:00Z".to_string(),
            file_path: file_path.to_string(),
            width: 2,
            height: 2,
            camera_id: None,
            duration: None,
            max_adu: None,
            cooler_setpoint_c: None,
            sensor_temperature_c: None,
            optics: None,
            sections: serde_json::Map::new(),
        }
    }

    async fn body_bytes(response: Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    #[tokio::test]
    async fn pixels_serves_u16_from_cache() {
        let cache = ImageCache::new(64, 4, std::path::PathBuf::from("/nonexistent"));
        cache.insert(
            "doc-1",
            cached_u16(ndarray::Array2::from_shape_vec((2, 2), vec![1u16, 2, 3, 4]).unwrap()),
        );
        let response =
            get_image_pixels(State(test_app_state(cache)), Path("doc-1".to_string())).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/imagebytes"
        );
        let body = body_bytes(response).await;
        assert_eq!(body.len(), IMAGEBYTES_HEADER_LEN + 4 * 2);
        assert_eq!(&body[24..28], &TRANSMISSION_U16.to_le_bytes());
        assert_eq!(&body[44..52], &[1, 0, 2, 0, 3, 0, 4, 0]);
    }

    #[tokio::test]
    async fn pixels_serves_i32_from_cache() {
        let cache = ImageCache::new(64, 4, std::path::PathBuf::from("/nonexistent"));
        cache.insert(
            "doc-1",
            cached_i32(ndarray::Array2::from_shape_vec((2, 2), vec![1i32, 2, 3, 4]).unwrap()),
        );
        let response =
            get_image_pixels(State(test_app_state(cache)), Path("doc-1".to_string())).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/imagebytes"
        );
        let body = body_bytes(response).await;
        assert_eq!(body.len(), IMAGEBYTES_HEADER_LEN + 4 * 4);
        assert_eq!(&body[24..28], &TRANSMISSION_I32.to_le_bytes());
        let mut expected = Vec::new();
        for v in [1i32, 2, 3, 4] {
            expected.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(&body[44..], &expected[..]);
    }

    #[tokio::test]
    async fn pixels_returns_404_on_cache_miss() {
        // `ImageCache::resolve()` falls back to reloading from disk on an
        // in-memory miss. This test still returns 404 because both the
        // cache and the configured data directory (`/nonexistent`) miss
        // for the requested document.
        let response = get_image_pixels(
            State(test_app_state(ImageCache::new(
                64,
                4,
                std::path::PathBuf::from("/nonexistent"),
            ))),
            Path("missing".to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn metadata_reports_bitpix_16_for_u16_cached() {
        let cache = ImageCache::new(64, 4, std::path::PathBuf::from("/nonexistent"));
        cache.insert(
            "doc-1",
            cached_u16(ndarray::Array2::from_elem((2, 2), 0u16)),
        );

        let (status, Json(body)) =
            get_image_metadata(State(test_app_state(cache)), Path("doc-1".to_string())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["bitpix"], 16);
        assert_eq!(body["in_cache"], true);
    }

    #[tokio::test]
    async fn metadata_reports_bitpix_32_for_i32_cached() {
        let cache = ImageCache::new(64, 4, std::path::PathBuf::from("/nonexistent"));
        cache.insert(
            "doc-1",
            cached_i32(ndarray::Array2::from_elem((2, 2), 0i32)),
        );

        let (status, Json(body)) =
            get_image_metadata(State(test_app_state(cache)), Path("doc-1".to_string())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["bitpix"], 32);
        assert_eq!(body["in_cache"], true);
    }

    #[tokio::test]
    async fn metadata_returns_doc_with_null_bitpix_when_max_adu_null() {
        // Pins the contract: when a sidecar+FITS pair exists on disk but
        // the sidecar has `max_adu: null`, `resolve()` declines (the cache
        // can't pick a CachedPixels variant), yet the metadata route
        // still returns 200 via the `resolve_document()` fallback. The
        // pixels-bearing fields (`bitpix`, `in_cache`) reflect that pixels
        // were not rehydrated. Mirrors the symmetric cache.rs test at
        // persistence::cache::tests for `resolve_document` itself.
        let dir = tempfile::tempdir().unwrap();
        let doc_uuid = "44444444-4444-4444-4444-444444444444";
        let uuid8 = &doc_uuid[..8];
        let fits_path = dir.path().join(format!("{uuid8}.fits"));
        crate::persistence::write_fits_u16(&fits_path, &[0u16; 4], 2, 2, doc_uuid)
            .await
            .unwrap();
        let doc = ExposureDocument {
            target: None,
            frame_type: None,
            id: doc_uuid.to_string(),
            captured_at: "2026-04-30T00:00:00Z".to_string(),
            file_path: fits_path.to_string_lossy().into_owned(),
            width: 2,
            height: 2,
            camera_id: None,
            duration: None,
            max_adu: None,
            cooler_setpoint_c: None,
            sensor_temperature_c: None,
            optics: None,
            sections: serde_json::Map::new(),
        };
        std::fs::write(
            dir.path().join(format!("{uuid8}.json")),
            serde_json::to_vec(&doc).unwrap(),
        )
        .unwrap();

        let cache = ImageCache::new(64, 4, dir.path().to_path_buf());
        let (status, Json(body)) =
            get_image_metadata(State(test_app_state(cache)), Path(doc_uuid.to_string())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body["bitpix"].is_null(),
            "bitpix must be null for max_adu=null docs, got {:?}",
            body["bitpix"]
        );
        assert_eq!(body["in_cache"], false);
        assert_eq!(body["width"], 2);
        assert_eq!(body["height"], 2);
    }

    #[tokio::test]
    async fn metadata_returns_404_on_cache_miss() {
        let (status, _) = get_image_metadata(
            State(test_app_state(ImageCache::new(
                64,
                4,
                std::path::PathBuf::from("/nonexistent"),
            ))),
            Path("missing".to_string()),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn imagebytes_header_layout_u16() {
        let body = imagebytes(4, 3, TRANSMISSION_U16, |buf| {
            buf.extend_from_slice(&[1u8, 2, 3, 4]);
        });
        assert_eq!(body.len(), IMAGEBYTES_HEADER_LEN + 4);
        // metadata_version = 1
        assert_eq!(&body[0..4], &1i32.to_le_bytes());
        // data_start = 44
        assert_eq!(&body[16..20], &44i32.to_le_bytes());
        // image_element_type = 2
        assert_eq!(&body[20..24], &2i32.to_le_bytes());
        // transmission_element_type = 8 (U16)
        assert_eq!(&body[24..28], &8i32.to_le_bytes());
        // rank = 2
        assert_eq!(&body[28..32], &2i32.to_le_bytes());
        // dim1 = 4
        assert_eq!(&body[32..36], &4i32.to_le_bytes());
        // dim2 = 3
        assert_eq!(&body[36..40], &3i32.to_le_bytes());
        // dim3 = 0
        assert_eq!(&body[40..44], &0i32.to_le_bytes());
        // payload
        assert_eq!(&body[44..48], &[1u8, 2, 3, 4]);
    }

    #[test]
    fn imagebytes_header_layout_i32() {
        let body = imagebytes(2, 2, TRANSMISSION_I32, |_| {});
        assert_eq!(body.len(), IMAGEBYTES_HEADER_LEN);
        assert_eq!(&body[24..28], &2i32.to_le_bytes());
    }

    #[test]
    fn detect_gap_none_without_cursor() {
        // A fresh subscriber (no Last-Event-ID) never has a gap.
        assert!(detect_gap(None, Some(5)).is_none());
        assert!(detect_gap(None, None).is_none());
    }

    #[test]
    fn detect_gap_none_when_next_expected_event_retained() {
        // Cursor 5, oldest retained 6 → the next event (6) is still buffered.
        assert!(detect_gap(Some(5), Some(6)).is_none());
        // Cursor 0, oldest retained 1 → event 1 still buffered.
        assert!(detect_gap(Some(0), Some(1)).is_none());
    }

    #[test]
    fn detect_gap_some_when_history_evicted_past_cursor() {
        // Cursor 5, oldest retained 10 → events 6..=9 were evicted.
        let gap = detect_gap(Some(5), Some(10)).unwrap();
        assert_eq!(gap.requested_after, 5);
        assert_eq!(gap.oldest_available, 10);
        // Boundary: cursor 5, oldest 7 → event 6 evicted, still a gap.
        assert!(detect_gap(Some(5), Some(7)).is_some());
    }

    #[test]
    fn detect_gap_none_when_history_empty() {
        // Nothing retained → cannot assert a gap; the client just resumes live.
        assert!(detect_gap(Some(42), None).is_none());
    }

    #[test]
    fn detect_gap_does_not_overflow_on_max_cursor() {
        // A near-`u64::MAX` client cursor must not overflow-panic on `after + 1`.
        assert!(detect_gap(Some(u64::MAX), Some(10)).is_none());
        assert!(detect_gap(Some(u64::MAX - 1), Some(u64::MAX)).is_none());
    }

    #[test]
    fn last_event_id_from_header() {
        let mut headers = HeaderMap::new();
        headers.insert("last-event-id", "1247".parse().unwrap());
        let params = SubscribeParams {
            last_event_id: None,
        };
        assert_eq!(parse_last_event_id(&headers, &params), Some(1247));
    }

    #[test]
    fn last_event_id_header_wins_over_query() {
        let mut headers = HeaderMap::new();
        headers.insert("last-event-id", "1247".parse().unwrap());
        let params = SubscribeParams {
            last_event_id: Some(99),
        };
        assert_eq!(parse_last_event_id(&headers, &params), Some(1247));
    }

    #[test]
    fn last_event_id_falls_back_to_query() {
        let headers = HeaderMap::new();
        let params = SubscribeParams {
            last_event_id: Some(99),
        };
        assert_eq!(parse_last_event_id(&headers, &params), Some(99));
    }

    #[test]
    fn last_event_id_none_when_absent_or_unparseable() {
        let params = SubscribeParams {
            last_event_id: None,
        };
        assert_eq!(parse_last_event_id(&HeaderMap::new(), &params), None);
        // A non-numeric header is ignored (falls back to the absent query).
        let mut headers = HeaderMap::new();
        headers.insert("last-event-id", "not-a-number".parse().unwrap());
        assert_eq!(parse_last_event_id(&headers, &params), None);
    }

    /// Acceptance §3.3(3): a consumer that lags the broadcast channel is
    /// disconnected (server-initiated end-of-body) rather than backing it up.
    /// We subscribe, then overrun `BROADCAST_CAPACITY` (256) *before* the
    /// response body is polled, so the subscriber's first `recv()` returns
    /// `Lagged`; the handler must emit a final `stream_gap` (carrying the
    /// `lagged` count) and end the stream. `to_bytes` completing at all proves
    /// the body ended — a backed-up stream would hang here.
    #[tokio::test]
    async fn lagged_consumer_gets_stream_gap_then_disconnect() {
        use axum::response::IntoResponse;

        let state = test_app_state(ImageCache::new(64, 4, PathBuf::from("/tmp")));
        let bus = state.mcp.event_bus.clone();
        let response = subscribe_events(
            State(state),
            HeaderMap::new(),
            Query(SubscribeParams {
                last_event_id: None,
            }),
        )
        .await
        .into_response();

        // The receiver was created inside `subscribe_events`; flooding it now
        // (before the body is read) overruns the channel deterministically.
        for i in 0..400 {
            bus.emit("stress_event", serde_json::json!({ "i": i }));
        }

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains("event: stream_gap"),
            "a lagged consumer must receive a stream_gap frame; body was: {text:?}"
        );
        assert!(
            text.contains("\"lagged\""),
            "the lag stream_gap must carry the lagged count; body was: {text:?}"
        );
    }

    // --- /api/config REST endpoints ------------------------------------------

    fn config_with_camera_password(password: &str) -> crate::config::Config {
        serde_json::from_value(serde_json::json!({
            "session": { "data_directory": "/tmp/rp-test" },
            "equipment": {
                "cameras": [{
                    "id": "main-cam",
                    "alpaca_url": "http://127.0.0.1:1",
                    "auth": { "username": "obs", "password": password }
                }]
            },
            "server": { "port": 0 }
        }))
        .unwrap()
    }

    /// Persist `config` to a fresh temp file and build an `AppState` whose
    /// running config and config path both point at it — the state rp is in
    /// right after booting from that file.
    fn config_test_state(config: crate::config::Config) -> (AppState, tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rp.json");
        rusty_photon_config::save(&path, &serde_json::to_value(&config).unwrap()).unwrap();
        let state = test_app_state_with_config(
            ImageCache::new(64, 4, PathBuf::from("/tmp")),
            config,
            path.clone(),
        );
        (state, dir, path)
    }

    async fn response_json(response: Response) -> Value {
        serde_json::from_slice(&body_bytes(response).await).unwrap()
    }

    #[tokio::test]
    async fn config_get_redacts_device_password_and_lists_no_overrides() {
        let (state, _dir, _path) = config_test_state(config_with_camera_password("hunter2"));
        let response = get_config(State(state)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(
            body.pointer("/config/equipment/cameras/0/auth/password")
                .and_then(Value::as_str),
            Some(crate::config_actions::REDACTED)
        );
        assert_eq!(body["overrides"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn config_schema_lists_server_port_read_only() {
        let response = get_config_schema().await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["read_only_fields"], serde_json::json!(["server.port"]));
        assert_eq!(body["locked_fields"], serde_json::json!([]));
        assert!(
            body.pointer("/schema/properties/server").is_some(),
            "schema must describe the config shape"
        );
    }

    #[tokio::test]
    async fn config_put_change_is_restart_required_with_status_ok() {
        let (state, _dir, path) = config_test_state(scaffold_config());
        let mut submitted = serde_json::to_value(&*state.config).unwrap();
        submitted["imaging"]["cache_max_mib"] = serde_json::json!(256);

        let response = put_config(State(state), submitted.to_string()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["status"], "ok", "restart disposition never 'applying'");
        assert_eq!(
            body["restart_required"],
            serde_json::json!(["imaging.cache_max_mib"])
        );
        assert_eq!(body["reload"], serde_json::json!([]));
        assert_eq!(
            body["persisted_to"].as_str(),
            Some(path.display().to_string().as_str())
        );

        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            on_disk
                .pointer("/imaging/cache_max_mib")
                .and_then(Value::as_u64),
            Some(256)
        );
    }

    #[tokio::test]
    async fn config_put_unchanged_is_ok_with_empty_lists() {
        let (state, _dir, _path) = config_test_state(scaffold_config());
        let submitted = serde_json::to_value(&*state.config).unwrap();

        let response = put_config(State(state), submitted.to_string()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["status"], "ok");
        assert_eq!(body["restart_required"], serde_json::json!([]));
        assert_eq!(body["reload"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn config_put_invalid_site_latitude_is_domain_error_and_file_untouched() {
        let (state, _dir, path) = config_test_state(scaffold_config());
        let file_before = std::fs::read_to_string(&path).unwrap();
        let mut submitted = serde_json::to_value(&*state.config).unwrap();
        submitted["site"] =
            serde_json::json!({ "latitude_degrees": 91.0, "longitude_degrees": 0.0 });

        let response = put_config(State(state), submitted.to_string()).await;
        // Domain error, not transport error: HTTP 200 with status "invalid".
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["status"], "invalid");
        assert_eq!(body["errors"][0]["path"], "site.latitude_degrees");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            file_before,
            "an invalid apply must leave the file byte-identical"
        );
    }

    #[tokio::test]
    async fn config_put_malformed_body_is_400_plain_text() {
        let (state, _dir, path) = config_test_state(scaffold_config());
        let file_before = std::fs::read_to_string(&path).unwrap();

        let response = put_config(State(state), "this is not json".to_string()).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let text = String::from_utf8(body_bytes(response).await).unwrap();
        assert!(
            text.contains("invalid config JSON"),
            "400 body must say what went wrong, got: {text:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), file_before);
    }

    #[tokio::test]
    async fn config_put_persist_failure_is_500() {
        // The config path is a *directory*: validation passes, the atomic
        // persist fails — the handler's internal-error branch.
        let dir = tempfile::tempdir().unwrap();
        let state = test_app_state_with_config(
            ImageCache::new(64, 4, PathBuf::from("/tmp")),
            scaffold_config(),
            dir.path().to_path_buf(),
        );
        let mut submitted = serde_json::to_value(&*state.config).unwrap();
        submitted["imaging"]["cache_max_mib"] = serde_json::json!(512);

        let response = put_config(State(state), submitted.to_string()).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn config_put_sentinel_round_trip_keeps_stored_password_on_disk() {
        let (state, _dir, path) = config_test_state(config_with_camera_password("hunter2"));
        // Submit what GET /api/config returned: the password redacted.
        let mut submitted = serde_json::to_value(&*state.config).unwrap();
        submitted["equipment"]["cameras"][0]["auth"]["password"] =
            serde_json::json!(crate::config_actions::REDACTED);

        let response = put_config(State(state), submitted.to_string()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["status"], "ok");
        assert_eq!(body["restart_required"], serde_json::json!([]));

        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            on_disk
                .pointer("/equipment/cameras/0/auth/password")
                .and_then(Value::as_str),
            Some("hunter2"),
            "the sentinel must keep the stored secret, never replace it"
        );
    }

    #[tokio::test]
    async fn config_put_removing_the_first_camera_keeps_the_seconds_password_on_disk() {
        let config: crate::config::Config = serde_json::from_value(serde_json::json!({
            "session": { "data_directory": "/tmp/rp-test" },
            "equipment": {
                "cameras": [
                    {
                        "id": "cam-one",
                        "alpaca_url": "http://127.0.0.1:1",
                        "auth": { "username": "obs", "password": "first-secret" }
                    },
                    {
                        "id": "cam-two",
                        "alpaca_url": "http://127.0.0.1:1",
                        "auth": { "username": "obs", "password": "second-secret" }
                    }
                ]
            },
            "server": { "port": 0 }
        }))
        .unwrap();
        let (state, _dir, path) = config_test_state(config);
        // The equipment page's delete flow: PUT the redacted config with
        // camera 0 removed, which shifts cam-two down to index 0. The
        // sentinel must pair with cam-two's stored password by id, not with
        // the deleted camera's by position.
        let mut submitted = serde_json::to_value(&*state.config).unwrap();
        submitted["equipment"]["cameras"]
            .as_array_mut()
            .unwrap()
            .remove(0);
        submitted["equipment"]["cameras"][0]["auth"]["password"] =
            serde_json::json!(crate::config_actions::REDACTED);

        let response = put_config(State(state), submitted.to_string()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["status"], "ok");

        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            on_disk
                .pointer("/equipment/cameras/0/id")
                .and_then(Value::as_str),
            Some("cam-two")
        );
        assert_eq!(
            on_disk
                .pointer("/equipment/cameras/0/auth/password")
                .and_then(Value::as_str),
            Some("second-secret"),
            "the surviving camera must keep its own password, not the deleted one's"
        );
    }

    #[tokio::test]
    async fn config_put_unwritable_path_is_500() {
        // A read/persist failure surfaces as 500, not a panic. A regular
        // *file* as the path's parent fails on every platform (and even as
        // root, unlike permission-based setups).
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, "x").unwrap();
        let state = test_app_state_with_config(
            ImageCache::new(64, 4, PathBuf::from("/tmp")),
            scaffold_config(),
            blocker.join("rp.json"),
        );
        let submitted = serde_json::to_value(&*state.config).unwrap();
        let response = put_config(State(state), submitted.to_string()).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}

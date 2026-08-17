//! Test-only Server-Sent-Events fixture routes for the `@browser` SSE spike
//! (UI-testing plan §9 Tier 2). Compiled ONLY under the `test-sse` cargo feature
//! — they ship nothing in the real binary.
//!
//! This is the streaming-risk spike the plan calls out as the **#2 infra risk**,
//! made real. It proves two things the cheaper layers cannot:
//!
//! 1. **Async server-pushed DOM updates are browser-observable.** One EventSource
//!    (`sse-connect`) feeds *two* regions via named events (`sse-swap`); both
//!    regions updating from that single connection is something only a real
//!    browser running the htmx SSE extension can establish — there are no server
//!    "bytes" for P1/P2 to assert, the updates arrive over a live stream after the
//!    page has loaded.
//! 2. **An open SSE stream still allows a graceful, coverage-flushing shutdown —
//!    *if* the browser is quit first.** Unlike a normal request, an SSE stream
//!    never closes on the shutdown signal (axum issue #2673; even `KeepAlive`
//!    doesn't end it), so axum's `with_graceful_shutdown` blocks until the held
//!    connection drops. The connection is held by the *out-of-process* browser, so
//!    there is no in-process escape hatch (no `world.mcp_client = None` equivalent
//!    — see `docs/skills/testing.md` §5.4): `driver.quit()` is the only lever, and
//!    it **must precede** `ServiceHandle::stop()`, or the BFF is SIGKILLed at the
//!    5s grace and silently loses its `.profraw` coverage. The teardown scenario
//!    asserts the BFF stops well within that grace once the browser is quit.
//!
//! The whole module is `#[coverage(off)]` (applied at the `pub mod sse_fixtures`
//! declaration in [`crate`]): test-only code must not count toward product
//! coverage, even though the `--all-features` coverage build compiles it.

use std::convert::Infallible;
use std::time::Duration;

use async_stream::stream;
use axum::http::header;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use maud::{html, Markup};

use crate::pages;
use crate::AppState;

/// The vendored htmx Server-Sent-Events extension. htmx 2.0 split SSE out of core,
/// so `hx-ext="sse"` needs this extension loaded alongside the vendored htmx 2.0.4
/// `htmx.min.js`. Vendored **byte-for-byte** (so it diffs cleanly against upstream)
/// from `https://unpkg.com/htmx-ext-sse@2.2.3/dist/sse.js` — the `htmx-ext-sse`
/// package v2.2.3 from `github.com/bigskysoftware/htmx-extensions`, the 2.x
/// extension line that pairs with htmx 2.x. The htmx project is published under the
/// Zero-Clause BSD (0BSD) license (the extensions repo ships no separate LICENSE
/// file; htmx core's `LICENSE` is 0BSD). `include_str!`'d **here**, in the
/// `test-sse`-gated module, so it never embeds in the real binary the way
/// `assets::HTMX_JS` does.
const SSE_EXTENSION_JS: &str = include_str!("../assets/htmx-ext-sse.js");

/// The test-only SSE fixture routes, merged into the BFF router when the `test-sse`
/// feature is on (see [`crate::build_router`]). They carry no state, so the router
/// stays generic over [`AppState`].
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/fixtures/sse", get(sse_page))
        .route("/fixtures/sse/stream", get(sse_stream))
        .route("/fixtures/sse.js", get(sse_extension_js))
}

/// The SSE fixture page: one `sse-connect` `EventSource` feeding two `sse-swap`
/// regions. Both must update from the single connection (browser-only proof).
async fn sse_page() -> Markup {
    pages::layout(
        "SSE fixture",
        &html! {
            // Load the htmx SSE extension AFTER htmx core (which `layout` puts in
            // <head>, so it runs first). This body <script> executes during parsing
            // — before DOMContentLoaded, i.e. before htmx's initial scan — so the
            // extension is registered by the time htmx processes `hx-ext="sse"`.
            script src="/fixtures/sse.js" {}
            div hx-ext="sse" sse-connect="/fixtures/sse/stream" {
                div id="region-a" sse-swap="region-a" { "initial a" }
                div id="region-b" sse-swap="region-b" { "initial b" }
            }
        },
    )
}

/// The streaming endpoint: emit two named events on a short timer (genuinely pushed
/// *after* the browser's `EventSource` connects, not bundled into the initial
/// response), then hold the connection open forever. The open stream is the
/// decisive teardown hazard — see the module docs / axum #2673.
async fn sse_stream() -> impl IntoResponse {
    let body = stream! {
        tokio::time::sleep(Duration::from_millis(50)).await;
        yield Ok::<_, Infallible>(Event::default().event("region-a").data("alpha pushed"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        yield Ok(Event::default().event("region-b").data("beta pushed"));
        // Never completes, so the SSE response never finishes on its own: the
        // browser holds this connection open until it is quit. `KeepAlive` injects
        // comment heartbeats meanwhile (mirroring what the real telemetry stream
        // will do), but it does NOT end the stream.
        std::future::pending::<()>().await;
    };
    Sse::new(body).keep_alive(KeepAlive::default())
}

/// Serve the vendored htmx SSE extension (test-only route, present only with the
/// `test-sse` feature).
async fn sse_extension_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        SSE_EXTENSION_JS,
    )
}

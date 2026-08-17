//! Test-only `/fixtures/*` routes for the `@browser` BDD layer (UI-testing plan
//! §9 Tier 1). Compiled ONLY under the `test-fixtures` cargo feature — they ship
//! nothing in the real binary.
//!
//! Each fixture exercises an htmx behavior the **server-bytes** layers (P1 DOM
//! assertions, P2 byte snapshots) cannot observe, so it proves the browser
//! harness sees a divergence the cheaper layers miss:
//!
//! - **`hx-swap-oob`** — one response updates *two* regions: the `hx-target` plus
//!   an out-of-band sibling. The positive and negative cases serve **byte-identical
//!   fragments**, so P1/P2 cannot tell them apart; only the browser proves a
//!   *second* region actually updated, and that the same OOB element is silently
//!   dropped when the page has no matching target. The divergence is in the *page
//!   DOM*, not the bytes.
//! - **`HX-Retarget` / `HX-Reswap`** — response *headers* move a **byte-identical**
//!   body to a different target and swap it differently. P1/P2 compare the body and
//!   see nothing; the retarget/reswap live entirely in the headers (a §A
//!   header-presence tripwire) and the landing is only browser-observable.
//! - **`HX-Push-Url`** — a response header changes the browser URL/history without
//!   a navigation — observable only in the browser.
//!
//! The whole module is `#[coverage(off)]` (applied at the `pub mod fixtures`
//! declaration in [`crate`]): test-only code must not count toward product
//! coverage, even though the `--all-features` coverage build compiles it.

use axum::http::header::{HeaderName, HeaderValue};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use maud::{html, Markup};

use crate::pages;
use crate::AppState;

/// The single trigger button id shared by every fixture page, so the BDD step
/// "I click the fixture button" has one stable selector.
const TRIGGER_ID: &str = "trigger";

/// The test-only fixture routes, merged into the BFF router when the
/// `test-fixtures` feature is on (see [`crate::build_router`]). They carry no
/// state, so the router stays generic over [`AppState`].
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/fixtures/oob", get(oob_page))
        .route("/fixtures/oob/swap", get(oob_swap))
        .route("/fixtures/oob-missing", get(oob_missing_page))
        .route("/fixtures/oob-missing/swap", get(oob_missing_swap))
        .route("/fixtures/retarget", get(retarget_page))
        .route("/fixtures/retarget/swap", get(retarget_swap))
        .route("/fixtures/push-url", get(push_url_page))
        .route("/fixtures/push-url/swap", get(push_url_swap))
}

/// The OOB fragment served by both the positive and the missing-target swap: a
/// `<span>` for the `hx-target` plus an out-of-band `#toast-region` div. Whether
/// the toast lands depends on the *page's* DOM (does `#toast-region` exist?), not
/// on these bytes — the exact divergence only a browser can observe.
fn oob_fragment() -> Markup {
    html! {
        span { "swapped main" }
        div id="toast-region" hx-swap-oob="true" { "swapped toast" }
    }
}

/// hx-swap-oob, positive: the page HAS a `#toast-region`, so htmx swaps the main
/// fragment into `#main-region` AND the OOB element into `#toast-region`.
async fn oob_page() -> Markup {
    pages::layout(
        "OOB fixture",
        &html! {
            div id="main-region" { "initial main" }
            div id="toast-region" { "initial toast" }
            button id=(TRIGGER_ID)
                hx-get="/fixtures/oob/swap"
                hx-target="#main-region"
                hx-swap="innerHTML" { "Go" }
        },
    )
}

async fn oob_swap() -> Markup {
    oob_fragment()
}

/// hx-swap-oob, negative: the page has NO `#toast-region`, so htmx has nowhere to
/// put the OOB element and silently drops it — the main swap still lands.
async fn oob_missing_page() -> Markup {
    pages::layout(
        "OOB missing-target fixture",
        &html! {
            div id="main-region" { "initial main" }
            button id=(TRIGGER_ID)
                hx-get="/fixtures/oob-missing/swap"
                hx-target="#main-region"
                hx-swap="innerHTML" { "Go" }
        },
    )
}

async fn oob_missing_swap() -> Markup {
    oob_fragment()
}

/// HX-Retarget + HX-Reswap: the page declares `hx-target="#primary"` and
/// `hx-swap="outerHTML"`, but the response headers retarget the swap to
/// `#secondary` and re-swap it as `innerHTML`. The body is a plain fragment
/// identical to a normal swap — the divergence is entirely in the headers. Both
/// headers are load-bearing here: the retarget moves it off `#primary`, and the
/// reswap-to-`innerHTML` is why `#secondary` *survives* with new content (the
/// page's `outerHTML` would instead replace the `#secondary` element outright, and
/// the `#secondary`-shows assertion would then fail to find it).
async fn retarget_page() -> Markup {
    pages::layout(
        "Retarget fixture",
        &html! {
            div id="primary" { "initial primary" }
            div id="secondary" { "initial secondary" }
            button id=(TRIGGER_ID)
                hx-get="/fixtures/retarget/swap"
                hx-target="#primary"
                hx-swap="outerHTML" { "Go" }
        },
    )
}

async fn retarget_swap() -> Response {
    (
        [
            (
                HeaderName::from_static("hx-retarget"),
                HeaderValue::from_static("#secondary"),
            ),
            (
                HeaderName::from_static("hx-reswap"),
                HeaderValue::from_static("innerHTML"),
            ),
        ],
        // A plain fragment, byte-identical to what a normal swap would carry —
        // nothing here reveals the retarget/reswap; only the headers above do.
        Html("retargeted content"),
    )
        .into_response()
}

/// HX-Push-Url: the response header pushes a new URL into history (no navigation),
/// so the browser's location changes while the swap lands in `#main-region`.
async fn push_url_page() -> Markup {
    pages::layout(
        "Push-URL fixture",
        &html! {
            div id="main-region" { "initial main" }
            button id=(TRIGGER_ID)
                hx-get="/fixtures/push-url/swap"
                hx-target="#main-region"
                hx-swap="innerHTML" { "Go" }
        },
    )
}

async fn push_url_swap() -> Response {
    (
        [(
            HeaderName::from_static("hx-push-url"),
            HeaderValue::from_static("/fixtures/pushed"),
        )],
        Html("pushed main"),
    )
        .into_response()
}

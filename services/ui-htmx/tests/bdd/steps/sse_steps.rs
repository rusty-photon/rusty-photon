//! Step definitions for sse.feature (`@browser`) — Layer C / P3.
//!
//! These drive the test-only `/fixtures/sse` routes (present only in a
//! `test-sse`-feature binary) to prove the browser harness observes async
//! server-pushed DOM updates over a live Server-Sent-Events stream — and that an
//! open SSE stream still permits a graceful, coverage-flushing BFF shutdown when
//! the browser is quit first.
//!
//! Almost every verb is reused: the BFF-up `Given` and the fixture-load /
//! region-shows steps come from [`super::fixtures_steps`]; the quit-then-stop
//! `When` and the graceful-shutdown `Then` come from [`super::browser_steps`]
//! (the same coverage invariant, now with an SSE connection held open). Only the
//! stream-pushed precondition below is new — it belongs in `When`-space (it gates
//! the teardown, it is not the scenario's outcome), so it cannot reuse the
//! `Then`-registered region-shows step.

use cucumber::when;

use crate::world::UiWorld;

/// Bounded DOM-poll budget (~5s at 100ms/iter): ample for the `EventSource` to open
/// and the first event to swap on a busy CI host, well under the nightly job's
/// `timeout-minutes` cap.
const MAX_POLLS: usize = 50;

#[when(regex = r#"^the SSE stream has pushed "([^"]*)" into the "([^"]+)" region$"#)]
async fn sse_pushed(world: &mut UiWorld, text: String, css: String) {
    // Block until the live stream has actually swapped `text` into `css`, proving
    // the EventSource is open and streaming *before* we test teardown — otherwise
    // the teardown could race a not-yet-connected stream and prove nothing.
    assert!(
        world
            .browser()
            .wait_text_contains(&css, &text, MAX_POLLS)
            .await,
        "the SSE stream never pushed {text:?} into {css} (page source:\n{})",
        world.browser().page_source().await
    );
}

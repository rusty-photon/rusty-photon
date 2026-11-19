//! Step definitions for fixtures.feature (`@browser`) — Layer C / P3. These
//! drive the test-only `/fixtures/*` routes (present only in a
//! `test-fixtures`-feature binary) to prove the browser harness observes htmx
//! behaviors the server-bytes layers cannot: out-of-band swaps, response-header
//! retargets, and push-url history changes.

use cucumber::{given, then, when};

use crate::world::UiWorld;

/// Bounded DOM-poll budget (~5s at 100ms/iter): ample for an htmx swap on a busy
/// CI host, well under the nightly job's `timeout-minutes` cap.
const MAX_POLLS: usize = 50;

/// The single trigger button every fixture page renders (id matches
/// `fixtures::TRIGGER_ID`).
const TRIGGER: &str = "#trigger";

#[given("the ui-htmx BFF is running")]
async fn bff_running(world: &mut UiWorld) {
    world.start_bff_only().await;
}

#[when(regex = r#"^I load the "([^"]+)" fixture in a browser$"#)]
async fn load_fixture(world: &mut UiWorld, path: String) {
    world.browser_goto(&path).await;
}

#[when("I click the fixture button")]
async fn click_trigger(world: &mut UiWorld) {
    world.browser().click(TRIGGER).await;
}

#[then(regex = r#"^the "([^"]+)" region shows "([^"]*)"$"#)]
async fn region_shows(world: &mut UiWorld, css: String, text: String) {
    assert!(
        world
            .browser()
            .wait_text_contains(&css, &text, MAX_POLLS)
            .await,
        "{css} never showed {text:?} (page source:\n{})",
        world.browser().page_source().await
    );
}

#[then(regex = r#"^the "([^"]+)" region still shows "([^"]*)"$"#)]
async fn region_still_shows(world: &mut UiWorld, css: String, text: String) {
    // A point read: a preceding `region shows` step already waited for the swap to
    // settle, so by now an unchanged region must still hold its original text.
    let actual = world.browser().text_of(&css).await;
    assert!(
        actual.contains(&text),
        "{css} should still show {text:?} but shows {actual:?}"
    );
}

#[then(regex = r#"^the text "([^"]*)" appears nowhere in the page$"#)]
async fn text_absent(world: &mut UiWorld, text: String) {
    let source = world.browser().page_source().await;
    assert!(
        !source.contains(&text),
        "expected {text:?} to be absent, but it is present in:\n{source}"
    );
}

#[then(regex = r#"^the browser URL contains "([^"]+)"$"#)]
async fn url_contains(world: &mut UiWorld, needle: String) {
    assert!(
        world.browser().wait_url_contains(&needle, MAX_POLLS).await,
        "browser URL never contained {needle:?}"
    );
}

#[then(regex = r#"^the "([^"]+)" response carries "([^"]+)" of "([^"]*)" with body "([^"]*)"$"#)]
async fn response_header_tripwire(
    world: &mut UiWorld,
    path: String,
    header: String,
    value: String,
    body: String,
) {
    // The Layer A header-presence tripwire: the divergence-carrying signal is in
    // the response header, while the body is a plain fragment a P2 snapshot
    // couldn't distinguish from a normal swap. (The browser steps above prove it
    // actually retargets; this proves the body alone is insufficient to know that.)
    let (header_value, actual_body) = world.fixture_response_header_and_body(&path, &header).await;
    assert_eq!(
        header_value.as_deref(),
        Some(value.as_str()),
        "{path} response is missing header {header}: {value:?}"
    );
    assert_eq!(
        actual_body, body,
        "{path} response body differs from the plain fragment"
    );
}

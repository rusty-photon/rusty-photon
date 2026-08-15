//! Step definitions for browser.feature (`@browser`) — Layer C / P3.
//!
//! These drive a real headless Firefox via `WebDriver` to prove behaviors only a
//! browser can establish: that `htmx.min.js` actually loads and executes the
//! declared swaps. They reuse the `Given a dsd-fp2 driver running …` setup from
//! [`super::config_page_steps`]; only the When/Then verbs here touch the browser.
//! The whole feature is gated behind `UI_BROWSER_TESTS=1` (see `tests/bdd.rs`).

use std::time::Duration;

use cucumber::{then, when};

use crate::world::UiWorld;

/// A generous bound for browser DOM polls: ~5s at 100ms/iter, well under the
/// nightly job's `timeout-minutes` cap but long enough for a real page load and
/// an htmx swap on a busy CI host.
const MAX_POLLS: usize = 50;

/// The BFF must shut down within this budget after the browser is quit.
/// bdd-infra's `stop()` SIGTERMs, waits 5s, then SIGKILLs; a graceful exit
/// returns in milliseconds, while a shutdown blocked on a held browser
/// connection returns only after the full 5s grace — having `SIGKILLed` the BFF
/// and skipped its `atexit` coverage flush (testing.md §5.4). 4s sits clearly
/// between the two: well above a real graceful stop, below the 5s SIGKILL point.
const GRACEFUL_STOP_BUDGET: Duration = Duration::from_secs(4);

#[when("I load the dsd-fp2 config page in a browser")]
async fn load_in_browser(world: &mut UiWorld) {
    let path = UiWorld::device_config_path();
    world.browser_goto(&path).await;
}

#[then("the browser renders the configuration form")]
async fn renders_form(world: &mut UiWorld) {
    // The Apply button is server-rendered inside the form; seeing it in the
    // *live* DOM proves Firefox fetched the page and rendered it.
    assert!(
        world
            .browser()
            .wait_present("button.primary", MAX_POLLS)
            .await,
        "the Apply button never appeared in the live DOM"
    );
}

#[when("I click the unlock link for cover_calibrator.unique_id")]
async fn click_unlock(world: &mut UiWorld) {
    // The unlock affordance is a `<button hx-get>` (plan §7: the no-JS `<a href>`
    // fallback was dropped), so the selector targets the button.
    world
        .browser()
        .click(r#"button[hx-get$="unlock=cover_calibrator.unique_id"]"#)
        .await;
}

#[then("the browser shows cover_calibrator.unique_id editable")]
async fn shows_unique_id_editable(world: &mut UiWorld) {
    // Proves real htmx executed the hx-get → outerHTML swap: the re-rendered
    // #config-card replaces the disabled identity input with an enabled one.
    assert!(
        world
            .browser()
            .wait_enabled(r#"input[name="cover_calibrator.unique_id"]"#, MAX_POLLS)
            .await,
        "cover_calibrator.unique_id never became editable after the htmx swap"
    );
}

// --- Tier 0 step 3: coverage invariant (graceful shutdown with a browser) ----

#[when("I quit the browser and then stop the BFF")]
async fn quit_browser_then_stop_bff(world: &mut UiWorld) {
    world.quit_browser_then_stop_bff().await;
}

#[then("the BFF shuts down gracefully before the 5s SIGKILL grace elapses")]
fn bff_graceful_shutdown(world: &mut UiWorld) {
    let elapsed = world
        .bff_stop_elapsed
        .expect("the BFF was not stopped in this scenario");
    assert!(
        elapsed < GRACEFUL_STOP_BUDGET,
        "BFF stop took {elapsed:?} (>= {GRACEFUL_STOP_BUDGET:?}); it was likely SIGKILLed at the \
         5s grace, which skips its atexit coverage flush"
    );
    // The timing assertion above is the active, runner-agnostic invariant — it
    // runs on every @browser run. The check below is an additional *direct* proof
    // (the BFF actually wrote a `.profraw`) that only fires when this @browser
    // scenario itself runs under coverage with COVERAGE_DIR set: a local
    // `UI_BROWSER_TESTS=1 cargo llvm-cov … --test bdd`, or a future nightly
    // @browser-under-coverage job. It is deliberately NOT wired into the required
    // bazel-coverage CI job, which never sets UI_BROWSER_TESTS — the plan keeps the
    // browser layer out of the required gate (§3/§8). So this branch is dormant in
    // current CI by design, not dead: it activates the moment @browser is run under
    // coverage. Skipped (COVERAGE_DIR unset) under plain `cargo test` / `bazel test`.
    if let Some(coverage_dir) = std::env::var_os("COVERAGE_DIR") {
        assert!(
            crate::browser::bff_profraw_flushed(&coverage_dir),
            "no non-empty ui-htmx-*.profraw in COVERAGE_DIR after a graceful BFF stop"
        );
    }
}

// --- Tier 0 step 4: worst-case orphan reaping + failure artifacts ------------

#[when("the geckodriver process is killed and the session is reaped")]
async fn crash_and_reap(world: &mut UiWorld) {
    world.crash_and_reap_browser().await;
}

#[then("the reaper leaves no orphaned browser processes")]
fn no_orphans(world: &mut UiWorld) {
    assert!(
        !world.session_pids_before.is_empty(),
        "no live browser processes were found before the crash, so the reaper proves nothing"
    );
    assert!(
        world.orphan_survivors.is_empty(),
        "browser processes survived the reaper: {:?}",
        world.orphan_survivors
    );
}

#[then("the failure artifacts were captured at an absolute path before the reap")]
fn artifacts_captured(world: &mut UiWorld) {
    let (png, html) = world
        .artifacts
        .as_ref()
        .expect("no failure artifacts were captured");
    for path in [png, html] {
        assert!(
            path.is_absolute(),
            "artifact path is not absolute: {}",
            path.display()
        );
        let len = std::fs::metadata(path).map_or(0, |m| m.len());
        assert!(
            len > 0,
            "failure artifact missing or empty: {}",
            path.display()
        );
    }
}

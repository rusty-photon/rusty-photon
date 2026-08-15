//! BDD entry point for `ui-htmx`. Each scenario spawns the real `ui-htmx`
//! binary and a real `dsd-fp2` driver (mock hardware) and drives the BFF over
//! HTTP, so the `bdd_infra::bdd_main!` macro is used (child-process spawning,
//! skipped under Miri). Both binaries must be pre-built with `--all-features`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unreachable,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::indexing_slicing
)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![allow(
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::unused_async,
    clippy::used_underscore_binding,
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::suboptimal_flops,
    clippy::too_many_lines,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::float_cmp,
    clippy::similar_names,
    clippy::struct_excessive_bools
)]

#[path = "bdd/browser.rs"]
mod browser;

#[path = "bdd/dom.rs"]
mod dom;

#[path = "bdd/snapshot.rs"]
mod snapshot;

#[path = "bdd/sse_client.rs"]
mod sse_client;

#[path = "bdd/world.rs"]
mod world;

#[path = "bdd/steps/mod.rs"]
mod steps;

bdd_infra::bdd_main! {
    use cucumber::World as _;
    use world::UiWorld;

    UiWorld::cucumber()
        .after(|_feature, _rule, _scenario, _finished, maybe_world| {
            Box::pin(async move {
                if let Some(world) = maybe_world {
                    // Teardown order is load-bearing (UI-testing plan §10 and
                    // testing.md §5.4): quit the browser FIRST so the WebDriver
                    // session closes (and geckodriver tears Firefox down) before
                    // the BFF stops — a live session holds connections to the
                    // BFF open. Drop any open SSE reader for the same reason (an
                    // open /stream/events response blocks the BFF's graceful
                    // shutdown and silently loses its subprocess coverage). Then
                    // stop the BFF (so it stops calling rp / the driver), then
                    // the driver, then rp.
                    if let Some(browser) = world.browser.take() {
                        browser.quit().await;
                    }
                    world.sse = None;
                    if let Some(ui) = world.ui.as_mut() {
                        ui.stop().await;
                    }
                    if let Some(sentinel) = world.sentinel.as_mut() {
                        sentinel.stop().await;
                    }
                    if let Some(driver) = world.driver.as_mut() {
                        driver.stop().await;
                    }
                    if let Some(rp) = world.rp.as_mut() {
                        rp.stop().await;
                    }
                }
            })
        })
        // Scenario filter (the `_and_exit` variant: a bare `filter_run` lets
        // failures exit 0 under `harness = false` — see testing.md §2.7):
        //   * `@wip` is never run in the default suite (test-first artifacts).
        //   * `@browser` needs a real Firefox + geckodriver and is advisory;
        //     opt in with `UI_BROWSER_TESTS=1`. Gating on an env var (not a
        //     cargo feature) keeps browser flake out of the `--all-features`
        //     required gate while leaving `thirtyfour` always compiled.
        .filter_run_and_exit("tests/features", |feature, _rule, scenario| {
            let tagged = |tag: &str| {
                let want = tag.trim_start_matches('@');
                let matches = |t: &String| t.trim_start_matches('@') == want;
                feature.tags.iter().any(matches) || scenario.tags.iter().any(matches)
            };
            if tagged("wip") {
                return false;
            }
            if tagged("browser") && std::env::var("UI_BROWSER_TESTS").is_err() {
                return false;
            }
            true
        })
        .await;
}

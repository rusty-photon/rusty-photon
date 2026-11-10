//! BDD entry point for pa-falcon-rotator.
//!
//! Most scenarios run in-process: each builds a `BoundServer` on an
//! ephemeral port, holds a shared `Arc<MockFalconTransportFactory>` so
//! steps can drive mock state and inspect the wire-level command log,
//! and drives the Rotator / Switch trait surface via in-process Alpaca
//! HTTP clients. The setup-page, restart and auth scenarios spawn the
//! mock-feature binary through `ServiceHandle` instead, so the entry point
//! is `bdd_infra::bdd_main!` (docs/skills/testing.md §5.2).

#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::indexing_slicing
)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![allow(
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::unused_async,
    clippy::unused_async_trait_impl,
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

#[path = "bdd/world.rs"]
mod world;

#[path = "bdd/steps/mod.rs"]
mod steps;

bdd_infra::bdd_main! {
    use cucumber::World as _;
    use world::FalconRotatorWorld;

    FalconRotatorWorld::cucumber()
        // A step that matches no definition is `Skipped`, and a skipped
        // step passes: the scenario reports green having asserted nothing.
        // Fail the run on it instead (docs/skills/testing.md section 2.9).
        .fail_on_skipped()
        .after(|_feature, _rule, _scenario, _finished, maybe_world| {
            Box::pin(async move {
                if let Some(world) = maybe_world {
                    world.shutdown().await;
                }
            })
        })
        // Skip scenarios tagged `@wip` so an in-flight scenario can ride
        // on the branch without breaking the green-suite invariant; the tag
        // gets stripped in the commit that turns the scenario green. We use
        // `filter_run_and_exit` (NOT the bare `filter_run`) per
        // docs/skills/testing.md §2.7 — without `_and_exit` the binary
        // returns 0 even on scenario failures and CI silently passes on
        // broken scenarios (see issue #171).
        .filter_run_and_exit("tests/features", |feat, _rule, sc| {
            let is_wip = feat.tags.iter().any(|t| t == "wip" || t == "@wip")
                || sc.tags.iter().any(|t| t == "wip" || t == "@wip");
            !is_wip
        })
        .await;
}

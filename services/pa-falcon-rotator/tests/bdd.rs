//! BDD entry point for pa-falcon-rotator.
//!
//! Tests run in-process: each scenario builds a `BoundServer` on an
//! ephemeral port, holds a shared `Arc<MockFalconTransportFactory>` so
//! steps can drive mock state and inspect the wire-level command log,
//! and drives the Rotator / Switch trait surface via in-process Alpaca
//! HTTP clients. The `bdd_infra::bdd_main!` macro is **not** used here
//! — it's a Miri shim for BDD suites that spawn child processes via
//! `ServiceHandle`, which we don't.

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

#[tokio::main]
async fn main() {
    use cucumber::World as _;
    use world::FalconRotatorWorld;

    // Mirror the `bdd_main!` macro's first step: chdir to `BDD_PACKAGE_DIR`
    // when running under Bazel so `tests/features` resolves relative to the
    // package directory rather than the Bazel runfiles root. No-op under
    // `cargo test`, which doesn't set the env var.
    bdd_infra::__bdd_bazel_chdir();

    FalconRotatorWorld::cucumber()
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

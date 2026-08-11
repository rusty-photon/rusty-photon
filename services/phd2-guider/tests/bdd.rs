//! BDD test entry point for the phd2-guider HTTP service mode.
//!
//! The `@wip` filter follows `docs/skills/testing.md` §2.7: scenarios
//! land before their implementation carry the tag and are skipped at
//! runtime; the tag is removed in the commit that lands the
//! implementation. Both tag forms (with and without the leading `@`)
//! are accepted, matching `services/rp/tests/bdd.rs`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unreachable,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::indexing_slicing
)]

#[path = "bdd/world.rs"]
mod world;

#[path = "bdd/steps/mod.rs"]
mod steps;

bdd_infra::bdd_main! {
    use cucumber::World as _;
    use world::GuiderWorld;

    GuiderWorld::cucumber()
        .after(|_feature, _rule, _scenario, _finished, maybe_world| {
            Box::pin(async move {
                if let Some(world) = maybe_world {
                    if let Some(handle) = world.service_handle.as_mut() {
                        handle.stop().await;
                    }
                    // Drop kills the mock PHD2 child.
                    world.mock.take();
                }
            })
        })
        .filter_run_and_exit("tests/features", |feat, _rule, sc| {
            let is_wip = feat.tags.iter().any(|t| t == "wip" || t == "@wip")
                || sc.tags.iter().any(|t| t == "wip" || t == "@wip");
            !is_wip
        })
        .await;
}

//! BDD test entry point for sentinel service

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
    use world::SentinelWorld;

    SentinelWorld::cucumber()
        .after(|_feature, _rule, _scenario, _finished, maybe_world| {
            Box::pin(async move {
                if let Some(world) = maybe_world {
                    // Stop sentinel first so it can disconnect from filemonitor
                    if let Some(sentinel) = world.sentinel.as_mut() {
                        sentinel.stop().await;
                    }
                    if let Some(fm) = world.filemonitor.as_mut() {
                        fm.stop().await;
                    }
                    // Shut down the Pushover stub server, if this scenario started one.
                    if let Some(stub) = world.pushover_stub.take() {
                        stub.abort();
                    }
                    // Shut down the rp SSE stub (drops its connections), if any.
                    world.rp_event_stub = None;
                    // Shut down the corrective-ladder mount service stub, if any.
                    world.mount_stub = None;
                    // Shut down the flippable health stub, if any.
                    world.health_stub = None;
                }
            })
        })
        .run_and_exit("tests/features")
        .await;
}

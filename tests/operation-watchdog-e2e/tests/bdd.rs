//! BDD entry point for the operation-watchdog end-to-end suite.
//!
//! Spawns a real rp and a real sentinel (plus `OmniSim` and an in-process
//! plate-solver stub) and drives the watchdog through wedge → escalation →
//! corrective ladder. See `world.rs` for the process lifecycle.

#![allow(clippy::expect_used)]
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

bdd_infra::bdd_main! {
    use cucumber::World as _;
    use world::WatchdogE2eWorld;

    WatchdogE2eWorld::cucumber()
        .before(|_feature, _rule, _scenario, _world| {
            Box::pin(async move {
                // Reset OmniSim devices between scenarios (the singleton leaks
                // state otherwise). Non-fatal before the first scenario starts
                // OmniSim — the rp-unresponsive scenario never spawns it.
                let _ = bdd_infra::rp_harness::OmniSimHandle::reset_all_devices().await;
            })
        })
        .after(|_feature, _rule, _scenario, _finished, maybe_world| {
            Box::pin(async move {
                if let Some(world) = maybe_world {
                    world.teardown().await;
                }
            })
        })
        .run_and_exit("tests/features")
        .await;
}

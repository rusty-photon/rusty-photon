//! BDD test entry point for the polar-align service.
//!
//! These tests spawn `OmniSim`, rp, and polar-align (plus an in-process
//! plate-solver stub) and drive the polar-alignment workflow
//! end-to-end via rp's REST API and the plugin's /status endpoint.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::indexing_slicing
)]
#![allow(clippy::expect_used, clippy::panic)]
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
    use world::PolarAlignWorld;

    PolarAlignWorld::cucumber()
        .before(|_feature, _rule, _scenario, _world| {
            Box::pin(async move {
                // Reset every OmniSim device class before each
                // scenario. OmniSim is a per-process singleton;
                // without this, telescope state (park, tracking,
                // pointing) leaks between scenarios. Failures before
                // the suite's OmniSim exists are non-fatal inside the
                // helper; anything after is a loud panic.
                if let Err(errors) =
                    bdd_infra::rp_harness::OmniSimHandle::reset_all_devices().await
                {
                    panic!("OmniSim device reset failed: {}", errors.join("; "));
                }
            })
        })
        .after(|_feature, _rule, _scenario, _finished, maybe_world| {
            Box::pin(async move {
                if let Some(world) = maybe_world {
                    if let Some(pa) = world.polar_align.as_mut() {
                        pa.stop().await;
                    }
                    if let Some(rp) = world.rp.as_mut() {
                        rp.stop().await;
                    }
                }
            })
        })
        // Bazel sharding (BUILD `shard_count`): each shard process runs only
        // its deterministic slice of the scenarios, against its own private
        // OmniSim. Outside Bazel sharding this always passes.
        .filter_run_and_exit("tests/features", |feat, _rule, sc| {
            bdd_infra::sharding::scenario_in_current_shard(
                feat.path.as_deref(),
                &feat.name,
                sc.position.line,
            )
        })
        .await;
}

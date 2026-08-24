//! BDD test entry point for the session-runner service.
//!
//! These tests spawn three processes — `OmniSim`, rp, and session-runner —
//! and drive workflow documents end-to-end via rp's REST API.

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
    use world::SessionRunnerWorld;

    SessionRunnerWorld::cucumber()
        .before(|_feature, _rule, _scenario, _world| {
            Box::pin(async move {
                // Reset every OmniSim device class our scenarios touch to
                // defaults before each scenario — OmniSim is a per-process
                // singleton, so scenario N's state (cover position,
                // calibrator brightness, filter slot, camera config) would
                // otherwise leak into scenario N+1. Failures from the very
                // first scenario's hook (before any Given step has called
                // `OmniSimHandle::start()`) are non-fatal: connection-
                // refused against the default port is the expected case
                // there.
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
                    if let Some(sr) = world.session_runner.as_mut() {
                        sr.stop().await;
                    }
                    if let Some(rp) = world.rp.as_mut() {
                        rp.stop().await;
                    }
                    // Put back OmniSim's telescope site when a deep-sky
                    // scenario overwrote it with a computed one. The
                    // site is a profile setting the per-scenario
                    // restart does NOT reset, and on platforms without
                    // PR_SET_PDEATHSIG (macOS, Windows) the OmniSim
                    // process outlives this test binary — a leaked
                    // site fails rp's own planner scenarios, which pin
                    // their config to OmniSim's default site, on the
                    // reused instance.
                    if let Some((lat, lon)) = world.original_telescope_site {
                        bdd_infra::rp_harness::OmniSimHandle::set_telescope_site(lat, lon)
                            .await
                            .expect("failed to restore OmniSim's telescope site");
                    }
                }
            })
        })
        // `@wip` scenarios are durable design artifacts for behavior that
        // is not implemented yet (testing.md §2.7) — Phase D (triggers,
        // resume, safety) lands its scenarios ahead of the engine work.
        .filter_run_and_exit("tests/features", |feat, _rule, sc| {
            let is_wip = feat.tags.iter().any(|t| t == "wip" || t == "@wip")
                || sc.tags.iter().any(|t| t == "wip" || t == "@wip");
            // Bazel sharding (BUILD `shard_count`): each shard process runs
            // only its deterministic slice of the scenarios, against its own
            // private OmniSim. Outside Bazel sharding this always passes.
            let in_shard = bdd_infra::sharding::scenario_in_current_shard(
                feat.path.as_deref(),
                &feat.name,
                sc.position.line,
            );
            !is_wip && in_shard
        })
        .await;
}

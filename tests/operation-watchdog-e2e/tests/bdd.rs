//! BDD entry point for the operation-watchdog end-to-end suite.
//!
//! Spawns a real rp and a real sentinel (plus `OmniSim` and an in-process
//! plate-solver stub) and drives the watchdog through wedge → escalation →
//! corrective ladder. See `world.rs` for the process lifecycle.

// An allow, not an expect, for `clippy::panic`: whether it fires inside the
// `bdd_main!` body depends on the clippy version.
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
    use world::WatchdogE2eWorld;

    WatchdogE2eWorld::cucumber()
        // A step that matches no definition is `Skipped`, and a skipped
        // step passes: the scenario reports green having asserted nothing.
        // Fail the run on it instead (docs/skills/testing.md section 2.9).
        .fail_on_skipped()
        .before(|_feature, _rule, _scenario, _world| {
            Box::pin(async move {
                // The scenarios of this process share one OmniSim, so each
                // starts with every device class restarted to its defaults.
                // Until a step has started that OmniSim (the rp-unresponsive
                // scenario never starts it), the call sends nothing and
                // returns Ok. See docs/skills/testing.md § 5.5.
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
                    world.teardown().await;
                }
            })
        })
        .run_and_exit("tests/features")
        .await;
}

//! BDD test entry point for the calibrator-flats service.
//!
//! These tests spawn three processes — `OmniSim`, rp, and calibrator-flats —
//! and drive the flat calibration workflow end-to-end via rp's REST API.

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
    use world::CalibratorFlatsWorld;

    CalibratorFlatsWorld::cucumber()
        .before(|_feature, _rule, _scenario, _world| {
            Box::pin(async move {
                // Reset every OmniSim device class our scenarios touch
                // (telescope, camera, filter wheel, focuser, cover
                // calibrator) to defaults before each scenario. OmniSim
                // is a per-process singleton; without this, state from
                // scenario N (cover position, calibrator brightness,
                // filter slot, camera config) leaks into scenario N+1.
                // Each reset is a localhost PUT, run sequentially
                // (parallel resets raced OmniSim's unsynchronised
                // `AlpacaDevices` list — see `reset_all_devices` for
                // the writeup). We panic on any reset failure that
                // happens *after* the suite has started its OmniSim
                // — that's the loud-reset diagnostic from #172.
                // Failures from the very first scenario's hook
                // (before any Given step has called
                // `OmniSimHandle::start()`) are non-fatal:
                // connection-refused against the default port is the
                // expected case there.
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
                    if let Some(cf) = world.calibrator_flats.as_mut() {
                        cf.stop().await;
                    }
                    if let Some(rp) = world.rp.as_mut() {
                        rp.stop().await;
                    }
                }
            })
        })
        .run_and_exit("tests/features")
        .await;
}

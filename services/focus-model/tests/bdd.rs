//! BDD test entry point for the focus-model service.
//!
//! These tests spawn three processes — `OmniSim`, focus-model, and rp
//! with the provider registered — and call the focus tools end-to-end
//! through rp's MCP proxy.

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
    use world::FocusModelWorld;

    FocusModelWorld::cucumber()
        // A step that matches no definition is `Skipped`, and a skipped
        // step passes: the scenario reports green having asserted nothing.
        // Fail the run on it instead (docs/skills/testing.md section 2.9).
        .fail_on_skipped()
        .before(|_feature, _rule, _scenario, _world| {
            Box::pin(async move {
                // Reset every OmniSim device class our scenarios touch
                // (camera, filter wheel, focuser) to defaults before
                // each scenario. OmniSim is a per-process singleton;
                // without this, a scenario's focuser position and
                // filter slot leak into the next one. Failures from
                // the very first scenario's hook (before any Given
                // step has called `OmniSimHandle::start()`) are the
                // expected connection-refused case.
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
                    // Drop the streaming clients FIRST so rp's graceful
                    // shutdown can complete (testing.md §5.4): the
                    // scenario's client and any background caller.
                    world.mcp_client = None;
                    for (_, handle) in world.background_calls.drain(..) {
                        handle.abort();
                        // An abort only asks; the task still owns its
                        // client until it lands, and rp's shutdown
                        // would race that connection.
                        let _ = handle.await;
                    }
                    // rp holds the client session into the provider, so
                    // it goes first; the provider then has no inbound
                    // connection left to wait for.
                    if let Some(rp) = world.rp.as_mut() {
                        rp.stop().await;
                    }
                    if let Some(provider) = world.focus_model.as_mut() {
                        provider.stop().await;
                    }
                    if let Some(guider) = world.guider_stub.as_mut() {
                        guider.stop();
                    }
                }
            })
        })
        .run_and_exit("tests/features")
        .await;
}

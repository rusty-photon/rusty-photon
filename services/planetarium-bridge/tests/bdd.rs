//! BDD entry point. Spawns the planetarium-bridge binary and drives it
//! through the typed ASCOM Alpaca Telescope client plus a stub rp MCP server
//! (`bdd/stub_rp.rs`) — the harness the design doc's Testing section
//! specifies. Every feature is `@wip` until the device implementation lands
//! (development-workflow.md Phase 2).

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

#[path = "bdd/stub_rp.rs"]
mod stub_rp;

#[path = "bdd/steps/mod.rs"]
mod steps;

bdd_infra::bdd_main! {
    use cucumber::World as _;
    use world::BridgeWorld;

    BridgeWorld::cucumber()
        .after(|_feature, _rule, _scenario, _finished, maybe_world| {
            Box::pin(async move {
                if let Some(world) = maybe_world {
                    // Stop the bridge before the stub rp: a shutting-down
                    // bridge may still flush a spool replay at the stub, and
                    // the stub holds no connection into the bridge that could
                    // block its graceful shutdown.
                    if let Some(handle) = world.handle.as_mut() {
                        handle.stop().await;
                    }
                    if let Some(stub) = world.stub_rp.as_mut() {
                        stub.stop().await;
                    }
                }
            })
        })
        // Skip `@wip` scenarios so the suite ships ahead of the
        // implementation without breaking the green-suite invariant;
        // `_and_exit` makes a scenario failure a non-zero exit
        // (testing.md §2.7).
        .filter_run_and_exit("tests/features", |feature, _rule, scenario| {
            let is_wip = feature.tags.iter().any(|t| t == "wip" || t == "@wip")
                || scenario.tags.iter().any(|t| t == "wip" || t == "@wip");
            !is_wip
        })
        .await;
}

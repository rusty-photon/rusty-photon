//! BDD entry point for the dsd-fp2 driver.
//!
//! Scenarios spawn the dsd-fp2 binary (built with `--features mock` so the
//! `MockTransportFactory` is wired in place of the real serial transport)
//! and drive it through the typed ASCOM Alpaca `CoverCalibrator` client.
//! Matches the pattern used by `ppba-driver` and `qhy-focuser`.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::indexing_slicing
)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "bdd/world.rs"]
mod world;

#[path = "bdd/steps/mod.rs"]
mod steps;

bdd_infra::bdd_main! {
    use cucumber::World as _;
    use world::Fp2World;

    Fp2World::cucumber()
        .after(|_feature, _rule, _scenario, _finished, maybe_world| {
            Box::pin(async move {
                if let Some(world) = maybe_world {
                    if let Some(handle) = world.handle.as_mut() {
                        handle.stop().await;
                    }
                }
            })
        })
        .run_and_exit("tests/features")
        .await;
}

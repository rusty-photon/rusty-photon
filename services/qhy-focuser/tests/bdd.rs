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
    use world::QhyFocuserWorld;

    QhyFocuserWorld::cucumber()
        .after(|_feature, _rule, _scenario, _finished, maybe_world| {
            Box::pin(async move {
                if let Some(world) = maybe_world {
                    if let Some(handle) = world.focuser_handle.as_mut() {
                        handle.stop().await;
                    }
                }
            })
        })
        .run_and_exit("tests/features")
        .await;
}

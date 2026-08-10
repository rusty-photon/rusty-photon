//! BDD entry point. Spawns the zwo-camera binary (built with the `simulation`
//! backend) and drives it through the typed ASCOM Alpaca Camera client. The
//! binary must be pre-built with `--features simulation` (or `--all-features`).

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
    use world::CameraWorld;

    CameraWorld::cucumber()
        .after(|_feature, _rule, _scenario, _finished, maybe_world| {
            Box::pin(async move {
                if let Some(world) = maybe_world {
                    if let Some(handle) = world.handle.as_mut() {
                        handle.stop().await;
                    }
                }
            })
        })
        // Skip `@wip` scenarios so an in-flight feature can ride the branch
        // without breaking the green-suite invariant; `_and_exit` makes a
        // scenario failure a non-zero exit (testing.md §2.7).
        .filter_run_and_exit("tests/features", |feature, _rule, scenario| {
            let is_wip = feature.tags.iter().any(|t| t == "wip" || t == "@wip")
                || scenario.tags.iter().any(|t| t == "wip" || t == "@wip");
            !is_wip
        })
        .await;
}

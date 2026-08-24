//! BDD entry point. Spawns the svbony-camera binary (built with the
//! `simulation` backend) and drives it through the typed ASCOM Alpaca
//! Camera client. The binary must be pre-built with `--features simulation`
//! (or `--all-features`).
//!
//! All scenarios are green as of Phase E (docs/plans/archive/svbony-camera.md); the
//! `@wip` filter below is kept as the standard sanctioned mechanism
//! (docs/skills/testing.md §2.7) for any future feature landing ahead of
//! its implementation, not because anything is currently tagged.

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
        // Skip `@wip` scenarios so this design/BDD-scaffolding phase can land
        // without breaking the green-suite invariant; `_and_exit` makes a
        // scenario failure a non-zero exit (testing.md §2.7).
        .filter_run_and_exit("tests/features", |feature, _rule, scenario| {
            let is_wip = feature.tags.iter().any(|t| t == "wip" || t == "@wip")
                || scenario.tags.iter().any(|t| t == "wip" || t == "@wip");
            !is_wip
        })
        .await;
}

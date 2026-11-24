//! BDD test entry point for the guider.
//!
//! The scenarios start the `guider` binary against `guide-simulator-alpaca`'s
//! devices (docs/services/guider.md § Testing). Two tags filter scenarios
//! out:
//!
//! - `@wip` — a scenario that lands before the code that makes it pass,
//!   the convention of `docs/skills/testing.md` §2.7.
//!
//! - `@unix` — a scenario that depends on Unix file-permission semantics
//!   (a directory made read-only with `chmod`).
//!
//! Every filter accepts the tag with or without a leading `@`.

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
    use world::GuiderWorld;

    GuiderWorld::cucumber()
        // A step that matches no definition is `Skipped`, and a skipped
        // step passes: the scenario reports green having asserted nothing.
        // Fail the run on it instead (docs/skills/testing.md section 2.9).
        .fail_on_skipped()
        .after(|_feature, _rule, _scenario, _finished, maybe_world| {
            Box::pin(async move {
                if let Some(world) = maybe_world {
                    world.stop().await;
                }
            })
        })
        .filter_run_and_exit("tests/features", |feat, _rule, sc| {
            let tagged = |tag: &str| {
                feat.tags.iter().chain(sc.tags.iter()).any(|t| {
                    t.strip_prefix('@').unwrap_or(t) == tag
                })
            };
            !tagged("wip") && (!tagged("unix") || cfg!(unix))
        })
        .await;
}

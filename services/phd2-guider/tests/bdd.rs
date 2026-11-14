//! BDD test entry point for the phd2-guider HTTP service mode.
//!
//! The `@wip` filter follows `docs/skills/testing.md` §2.7: scenarios
//! land before their implementation carry the tag and are skipped at
//! runtime; the tag is removed in the commit that lands the
//! implementation. Scenarios tagged `@linux` run only on Linux, the one
//! platform with a keeper of PHD2, where a shell script stands in for
//! `systemctl`. Both tag forms (with and
//! without the leading `@`) are accepted, matching
//! `services/rp/tests/bdd.rs`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unreachable,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::indexing_slicing
)]
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

#[path = "bdd/phd2_unit.rs"]
mod phd2_unit;

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
                    if let Some(handle) = world.service_handle.as_mut() {
                        handle.stop().await;
                    }
                    // Drop kills the mock PHD2 child, and the unit's.
                    world.mock.take();
                    world.unit.take();
                }
            })
        })
        .filter_run_and_exit("tests/features", |feat, _rule, sc| {
            let tagged = |tag: &str| {
                let at = format!("@{tag}");
                feat.tags.iter().chain(sc.tags.iter()).any(|t| t == tag || *t == at)
            };
            // `@linux` scenarios stand in for systemd with a shell script on
            // the service's PATH, and expect a restart that only Linux has a
            // keeper for (`service::keeper::system_keeper`).
            !tagged("wip") && (!tagged("linux") || cfg!(target_os = "linux"))
        })
        .await;
}

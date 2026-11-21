//! BDD test entry point for plate-solver.
//!
//! Three tags filter scenarios out:
//!
//! - `@wip` — a scenario that lands before the code that makes it
//!   pass, the convention of `docs/skills/testing.md` §2.7.
//!
//! - `@requires-astap` — the real-ASTAP smoke, which runs only when
//!   `ASTAP_BINARY` is set in the environment. Pull-request jobs do
//!   not set it; the nightly smoke workflow does. See
//!   `docs/services/plate-solver.md` § Real-ASTAP Coverage Cadence.
//!
//! - `@unix` — a scenario that depends on Unix file-permission
//!   semantics (`chmod 000` to deny read). Denying a read on Windows
//!   takes DENY ACLs or file locks, not worth the complexity until a
//!   Windows-specific failure mode needs covering.
//!
//! Every filter accepts the tag with or without a leading `@`,
//! matching `services/rp/tests/bdd.rs`'s pattern (cucumber-rs may
//! strip the leading sigil depending on parser version).

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
    use world::PlateSolverWorld;

    PlateSolverWorld::cucumber()
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
                }
            })
        })
        .filter_run_and_exit("tests/features", |feat, _rule, sc| {
            let is_wip = feat.tags.iter().any(|t| t == "wip" || t == "@wip")
                || sc.tags.iter().any(|t| t == "wip" || t == "@wip");
            let needs_astap = feat
                .tags
                .iter()
                .any(|t| t == "requires-astap" || t == "@requires-astap")
                || sc
                    .tags
                    .iter()
                    .any(|t| t == "requires-astap" || t == "@requires-astap");
            let astap_available = std::env::var("ASTAP_BINARY").is_ok();
            let unix_only = feat.tags.iter().any(|t| t == "unix" || t == "@unix")
                || sc.tags.iter().any(|t| t == "unix" || t == "@unix");
            let is_unix = cfg!(unix);
            !is_wip && (!needs_astap || astap_available) && (!unix_only || is_unix)
        })
        .await;
}

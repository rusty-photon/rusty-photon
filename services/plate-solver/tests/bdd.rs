//! BDD test entry point for plate-solver.
//!
//! Two filter conditions:
//!
//! - `@wip` — Phase 3 lands feature files and step stubs before Phase
//!   4's HTTP server exists. All scenarios are tagged `@wip` until
//!   then; Phase 4 removes the tag in the same commit that lands the
//!   implementation. Convention per `docs/skills/testing.md` §2.7.
//!
//! - `@requires-astap` — gates a small cross-platform real-ASTAP
//!   smoke that fires only when `ASTAP_BINARY` is set in the
//!   environment. PR jobs do not set it; the dedicated nightly
//!   workflow does. See `docs/plans/archive/plate-solver.md`
//!   §"Real-ASTAP coverage: cadence and gating".
//!
//! Both filter forms accept the tag with or without a leading `@`,
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
            // `@unix` gates scenarios that depend on Unix-specific
            // filesystem semantics (e.g., `chmod 000` to deny read).
            // Windows file-permission denial requires DENY ACLs or
            // file locks — not worth the complexity until a real
            // Windows-specific failure mode surfaces.
            let unix_only = feat.tags.iter().any(|t| t == "unix" || t == "@unix")
                || sc.tags.iter().any(|t| t == "unix" || t == "@unix");
            let is_unix = cfg!(unix);
            // `@manual` gates scenarios that need an external fixture
            // not committed to the repo. Currently no scenarios are
            // tagged `@manual` (the M 101 happy-path was promoted in
            // issue #233); the filter is kept for future scenarios
            // that may rely on operator-staged inputs.
            // Run such scenarios by setting `RUN_MANUAL_BDD=1` and
            // staging the fixture into `tests/fixtures/`.
            let is_manual = feat.tags.iter().any(|t| t == "manual" || t == "@manual")
                || sc.tags.iter().any(|t| t == "manual" || t == "@manual");
            let manual_enabled = std::env::var("RUN_MANUAL_BDD").is_ok();
            !is_wip
                && (!needs_astap || astap_available)
                && (!unix_only || is_unix)
                && (!is_manual || manual_enabled)
        })
        .await;
}

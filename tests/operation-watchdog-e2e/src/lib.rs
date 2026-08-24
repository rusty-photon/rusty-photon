//! End-to-end harness for the operation watchdog (Sentinel) + the predictive
//! deadlines / real-time event stream (rp).
//!
//! This crate carries **no library code** — it exists only to host the
//! `tests/bdd.rs` cucumber suite, which spawns a real `rp` binary and a real
//! `sentinel` binary (plus `OmniSim` and an in-process plate-solver stub) and
//! drives the watchdog through wedge → escalation → corrective ladder. The
//! per-service BDD suites (`services/rp/tests`, `services/sentinel/tests`)
//! cover each half against stubs; this suite is the only place the two real
//! binaries run the full two-loop structure together.
//!
//! See `docs/services/sentinel.md` §Operation Watchdog and the archived plan
//! `docs/plans/archive/predictive-deadlines-and-watchdog.md`.

// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
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
        clippy::struct_excessive_bools,
    )
)]

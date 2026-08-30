//! Integration tests for QHYCCD simulation support
//!
//! These tests verify that simulated cameras work correctly without
//! requiring actual QHYCCD hardware.

#![cfg(feature = "simulation")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// Simulated-pixel and frame-geometry assertions cast and do arithmetic on the
// tests' own fixture values.
#![allow(clippy::as_conversions, clippy::arithmetic_side_effects)]
// Curated test-scope allow list — documented in the root Cargo.toml
// [workspace.lints] block.
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

mod common;
mod simulation;

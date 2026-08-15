//! FITS reader/writer wrapper used by every workspace consumer that
//! needs FITS I/O. Internally delegates reads to [`fitsrs`] and emits
//! writes via a hand-rolled pure-Rust serializer that supports BITPIX
//! 8/16/32 (integer) image HDUs.
//!
//! See `docs/decisions/001-fits-file-support.md` (Amendment A) for the
//! design rationale.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
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

pub mod atomic;
pub mod error;
pub mod reader;
pub mod writer;

pub use error::FitsError;

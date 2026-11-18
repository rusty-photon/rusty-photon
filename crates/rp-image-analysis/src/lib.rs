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

//! Pure image-analysis kernels: single-purpose math over `ArrayView2`.
//!
//! Each module is generic over [`pixel::Pixel`] and free of I/O, async
//! and persistence concerns. `rp` composes them into its measuring tools,
//! and the guider measures its guide stars with them. See
//! `docs/services/rp.md` (Module Structure) and `docs/services/guider.md`
//! (Star selection and measurement).

pub mod aperture;
pub mod background;
pub mod fwhm;
pub mod hfr;
pub mod pixel;
pub mod saturation;
pub mod snr;
pub mod stars;
pub mod stats;

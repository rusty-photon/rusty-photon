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

//! The guider's core, without I/O: frames in, measurements out.
//!
//! [`Frame`] reads a camera's frame for its sensor type and estimates its
//! background from a sample of its pixels. [`GuideStars`] selects the
//! guide stars of one frame and measures them in each frame after it:
//! which are usable, and the frame's offset, the median of theirs. See
//! `docs/services/guider.md` (Sensor handling, Star selection and
//! measurement).

mod background;
mod frame;
mod stars;

pub use background::{sampled_background, MIN_SAMPLE};
pub use frame::{Frame, FrameError, SensorType};
pub use stars::{
    FrameMeasurement, GuideStars, Offset, Position, SelectError, StarParams, StarReading, Usable,
};

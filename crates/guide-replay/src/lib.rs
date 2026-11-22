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

//! Guide-camera frames recorded on a rig, replayed through the guider's
//! measurement.
//!
//! [`Recording`] reads what `record.ps1` writes on the rig: the camera's
//! properties and each frame as the camera sent it. [`replay`] selects
//! guide stars on the first frame that has enough of them, measures every frame
//! after it as the guider does, and holds what it finds to a
//! [`Tolerance`]: whether the measurement keeps its stars, and whether
//! they agree with one another. See `docs/crates/guide-replay.md`.

mod imagebytes;
mod presence;
mod recording;
mod replay;
mod report;
#[cfg(test)]
mod test_support;

pub use imagebytes::{decode, DecodeError};
pub use recording::{Camera, Recorded, Recording, RecordingError};
pub use replay::{replay, write_csv, Reading, Replay, ReplayError, StarRow};
pub use report::{Failure, Report, StarReport, Tolerance};

//! `rp-vocabulary`: the shared, validated vocabulary of `rp`'s imaging
//! plan — the small domain value types ([`IcrsCoord`], [`Binning`],
//! [`FrameType`]) that `rp` and every surface that talks to it about plans
//! agree on.
//!
//! Exposure is deliberately *not* here: it is a plain [`std::time::Duration`]
//! (serialized with `humantime`, no custom format), and its filename-token
//! codec lives with the naming engine in `rp`. See ADR-019 / the crate doc.
//!
//! Each type is *parse-don't-validate*: a value that exists is valid by
//! construction, and that one constructor is the single validator every
//! surface shares. The crate holds no logic — no store, no template
//! engine, no ephemeris math, no protocol endpoints; only the validated
//! nouns those layers exchange.
//!
//! See [`docs/crates/rp-vocabulary.md`](https://github.com/rusty-photon/rusty-photon/blob/main/docs/crates/rp-vocabulary.md)
//! and ADR-019 for the design and the reasoning.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![deny(unsafe_code)]
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

mod binning;
mod coord;
mod frame_type;

pub use binning::{Binning, BinningParseError};
pub use coord::{CoordError, IcrsCoord};
pub use frame_type::{FrameType, FrameTypeParseError};

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

pub mod atomic;
pub mod error;
pub mod reader;
pub mod writer;

pub use error::FitsError;

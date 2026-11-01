//! Pure image-analysis kernels: single-purpose math over `ArrayView2`.
//!
//! Each submodule is generic over [`pixel::Pixel`] and free of I/O,
//! async, and persistence concerns. Compositional analyzers live in
//! [`crate::imaging::tools`]; cache and FITS I/O live in
//! [`crate::persistence`]. See `docs/services/rp.md` (Module Structure)
//! and `docs/plans/archive/image-evaluation-tools.md` for the layout rationale.

pub mod aperture;
pub mod background;
pub mod fwhm;
pub mod hfr;
pub mod pixel;
pub mod snr;
pub mod stars;
pub mod stats;

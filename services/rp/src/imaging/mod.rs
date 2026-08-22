//! Image analysis: pure kernels, compositional analyzers, and compound
//! equipment-driving tools.
//!
//! FITS I/O, the image cache, and
//! exposure-document storage live in [`crate::persistence`] —
//! [`analysis`] kernels and the pure compositional analyzers in
//! [`tools`] (`measure_basic`, `measure_stars`) are async- and
//! I/O-free so they can be unit-tested in isolation.
//!
//! Compound tools that drive equipment loops (`auto_focus` and the
//! planned `center_on_target`) also live under [`tools`]. They are
//! async and they take device-trait objects, so they are *not*
//! async-/I/O-free at the top level — but the math and grid logic
//! they bundle (`build_grid`, `fit_parabola`, `validate_params`) are
//! still pure helpers that test independently. The MCP wrappers in
//! `mcp.rs` provide concrete adapters that bind the equipment traits
//! to live Alpaca clients; tests substitute synthetic adapters.
//!
//! Submodules:
//! - [`analysis`]: single-purpose math (background, stars, hfr, fwhm,
//!   snr, stats, pixel trait). Pure, generic over [`Pixel`], no I/O.
//! - [`tools`]: compositional analyzers (`measure_basic`,
//!   `measure_stars`) plus compound equipment-driving tools
//!   (`auto_focus`).
//!
//! The flat re-exports below preserve the previous `crate::imaging::*`
//! call-site shape so MCP wiring doesn't have to know which submodule
//! a symbol lives in. See `docs/services/rp.md` (Module Structure) and
//! `docs/plans/archive/image-evaluation-tools.md` for the rationale.

pub mod analysis;
pub mod tools;

pub use analysis::background::{estimate_background, sigma_clipped_stats, BackgroundStats};
pub use analysis::fwhm::{fit_2d_gaussian, GaussianFit2D};
pub use analysis::hfr::{aggregate_hfr, star_hfr};
pub use analysis::pixel::Pixel;
pub use analysis::snr::{compute_snr, per_star_snr, SnrResult};
pub use analysis::stars::{detect_stars, DetectionParams, Star};
pub use analysis::stats::{compute_stats, ImageStats};
pub use tools::center_on_target::haversine_arcsec;
pub use tools::measure_basic::{measure_basic, MeasureBasicResult};
pub use tools::measure_stars::{
    measure_stars, MeasureStarsResult, StarMeasurement, DEFAULT_STAMP_HALF_SIZE,
};

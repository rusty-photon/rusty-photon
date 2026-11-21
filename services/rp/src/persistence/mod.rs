//! Persistence layer: FITS I/O, the unified image+document cache, and
//! exposure-document storage.
//!
//! The cache and the on-disk FITS+sidecar pair together form the
//! document store: a document is addressable by id as long as its files
//! sit in `<data_directory>`. The lazy filesystem fallback in
//! [`cache::ImageCache`] reads back entries that were evicted or
//! predate the current `rp` process.
//!
//! The pure image-analysis kernels live in [`rp_image_analysis`],
//! re-exported through [`crate::imaging`] — this module contains
//! everything that owns I/O, async, or on-disk layout, so the analysis
//! path stays unit-testable without a runtime.

pub mod cache;
pub mod document;
pub mod fits;
pub mod fits_header;

pub use cache::{CachedImage, CachedPixels, ImageCache, ResolvedDocument};
pub use document::{
    ExposureDocument, ExposureFocuser, ExposureGuiding, ExposureObservingConditions,
    ExposureRotator, ExposureSafetyMonitor, ExposureSite, ExposureTarget, ExposureTrainDevice,
    LightSource, MountPointing, Optics, OpticsInputs, PierSide, SensorType, RP_VERSION,
};
pub use fits::{
    read_fits_array, read_fits_doc_id, read_fits_pixels, write_fits_i32, write_fits_u16,
};

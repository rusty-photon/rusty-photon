//! Persistence layer: FITS I/O, the unified image+document cache, and
//! exposure-document storage.
//!
//! As of Phase 7 (`docs/plans/archive/image-evaluation-tools.md`), the cache and
//! the on-disk FITS+sidecar pair together form the document store: a
//! document is addressable by id as long as its files sit in
//! `<data_directory>`. The lazy filesystem fallback in
//! [`cache::ImageCache`] reads back entries that were evicted or
//! predate the current `rp` process.
//!
//! Pure image-analysis kernels live in [`crate::imaging`] — this module
//! contains everything that owns I/O, async, or on-disk layout, so the
//! analysis path stays unit-testable without a runtime.

pub mod cache;
pub mod document;
pub mod fits;

pub use cache::{CachedImage, CachedPixels, ImageCache};
pub use document::{
    read_sidecar_sync, sidecar_path, write_sidecar, write_sidecar_at, CameraGeometry,
    ExposureDocument, ExposureTarget, Optics,
};
pub use fits::{read_fits_doc_id, read_fits_pixels, write_fits_i32, write_fits_u16};

//! In-memory image cache.
//!
//! Holds the pixel buffer that `capture` already decoded so subsequent tools
//! (`measure_basic`, `auto_focus`, plugins) don't re-read and re-decode the
//! FITS file. The on-disk FITS file remains the durable source of truth — the
//! cache is strictly a hot-path optimization, with the file as fallback on
//! miss. See `docs/services/rp.md` (Image Cache) for the full design.
//!
//! Storage is `u16` for every consumer/prosumer astro camera (`max_adu` ≤
//! 65535); the `I32` variant is the hatch for future scientific cameras whose
//! `max_adu` exceeds 16-bit range.
//!
//! Eviction is LRU with two budgets — `cache_max_mib` and `cache_max_images`
//! — whichever trips first.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ndarray::Array2;
use tokio::sync::RwLock;
use tracing::debug;

use super::document::ExposureDocument;

/// Pixel storage variant.
///
/// The design intent is per-camera selection at
/// connect time (driven by the camera's `MaxADU`); the same camera always
/// reports the same `MaxADU`, so the variant is effectively per-camera. The
/// current implementation fetches `max_adu` per-frame in
/// `mcp.rs:capture` — see the "Phase 3 follow-up: stash `max_adu` on
/// `CameraEntry`" section in `docs/plans/archive/image-evaluation-tools.md`.
pub enum CachedPixels {
    U16(Array2<u16>),
    I32(Array2<i32>),
}

/// Dispatch on a `&CachedPixels` and run the body once per pixel variant.
///
/// Generic image-analysis functions are statically dispatched, so the runtime
/// tag in `CachedPixels` has to be unwrapped somewhere. This macro hides the
/// two-arm match while still emitting a separate monomorphization per pixel
/// type — the body is textually duplicated by the macro expander, then the
/// compiler monomorphizes each arm with `T = u16` or `T = i32`.
///
/// Caveat: because the body is duplicated before expansion, any non-`Copy`
/// value moved by the body would only compile in one arm. Capture by reference
/// or by `Copy` value to keep both arms valid.
#[macro_export]
macro_rules! dispatch_pixels {
    ($pixels:expr, |$arr:ident| $body:expr) => {
        match $pixels {
            $crate::persistence::CachedPixels::U16(__a) => {
                let $arr = __a.view();
                $body
            }
            $crate::persistence::CachedPixels::I32(__a) => {
                let $arr = __a.view();
                $body
            }
        }
    };
}

impl CachedPixels {
    /// Memory footprint of this buffer, in bytes.
    #[must_use]
    pub fn nbytes(&self) -> usize {
        match self {
            Self::U16(a) => a.len().saturating_mul(std::mem::size_of::<u16>()),
            Self::I32(a) => a.len().saturating_mul(std::mem::size_of::<i32>()),
        }
    }

    /// Build the right pixel variant from a flat i32 buffer based on the
    /// camera's declared `max_adu`. Used both by capture (post-readout) and
    /// by the disk-fallback resolver. When `max_adu ≤ 65535`, narrow to
    /// `u16` clamping each pixel into `[0, max_adu]` so a buggy driver
    /// can't introduce wrap-around. Otherwise keep the i32 buffer
    /// unchanged.
    ///
    /// Returns `None` if `from_shape_vec` fails (pixel count vs shape
    /// mismatch).
    pub fn from_i32_pixels(pixels: Vec<i32>, shape: (usize, usize), max_adu: u32) -> Option<Self> {
        if u16::try_from(max_adu).is_ok() {
            let max_cached = max_adu.cast_signed();
            // Clamped into [0, max_adu] and the guard proved max_adu
            // fits u16, so the conversion cannot fail.
            let narrowed: Vec<u16> = pixels
                .into_iter()
                .map(|p| u16::try_from(p.clamp(0, max_cached)).unwrap_or(u16::MAX))
                .collect();
            Array2::from_shape_vec(shape, narrowed).ok().map(Self::U16)
        } else {
            Array2::from_shape_vec(shape, pixels).ok().map(Self::I32)
        }
    }

    /// Build the U16 variant directly from a u16 buffer. Used by the
    /// capture path when `max_adu ≤ u16::MAX` to avoid the wasted
    /// i32→u16 round trip that `from_i32_pixels` would do.
    pub fn from_u16_pixels(pixels: Vec<u16>, shape: (usize, usize)) -> Option<Self> {
        Array2::from_shape_vec(shape, pixels).ok().map(Self::U16)
    }
}

/// A cached image plus the metadata tools need to make sense of it.
///
/// The exposure document lives inline behind a per-entry async `RwLock` so
/// section updates (`measure_basic` writing `image_analysis`, plugins writing
/// their own sections) can mutate it without contending with any other entry.
/// `json_nbytes` is the *serialized* size of the document — the cache budget
/// includes it because `detect_stars` / `measure_stars` per-star arrays push
/// document JSON into the tens-of-KB range, which is non-negligible relative
/// to a small u16 thumbnail.
pub struct CachedImage {
    pub pixels: CachedPixels,
    pub width: u32,
    pub height: u32,
    pub fits_path: PathBuf,
    pub max_adu: u32,
    pub document: RwLock<ExposureDocument>,
    /// Bytes returned by `serde_json::to_vec(&document)` at the last update.
    /// Updated atomically by `put_section` (Step 4) so the cache mutex can
    /// read it during eviction without taking the per-entry lock.
    pub json_nbytes: AtomicUsize,
}

impl CachedImage {
    /// Construct an entry, computing `json_nbytes` from the document up
    /// front. If JSON serialization fails (which would also break sidecar
    /// writes), `json_nbytes` falls back to `0` — the cache will under-bill
    /// the entry rather than refuse to insert. Surface the error elsewhere
    /// (sidecar write) so the caller can decide.
    #[must_use]
    pub fn new(
        pixels: CachedPixels,
        width: u32,
        height: u32,
        fits_path: PathBuf,
        max_adu: u32,
        document: ExposureDocument,
    ) -> Self {
        let json_nbytes = serde_json::to_vec(&document).map_or(0, |v| v.len());
        Self {
            pixels,
            width,
            height,
            fits_path,
            max_adu,
            document: RwLock::new(document),
            json_nbytes: AtomicUsize::new(json_nbytes),
        }
    }

    /// Total memory footprint counted against the cache budget: pixel bytes
    /// plus the last-known serialized document size.
    pub fn nbytes(&self) -> usize {
        self.pixels
            .nbytes()
            .saturating_add(self.json_nbytes.load(Ordering::Relaxed))
    }
}

/// Process-wide image cache. Cheap to clone — internally `Arc<Mutex<…>>`.
#[derive(Clone)]
pub struct ImageCache {
    inner: Arc<Mutex<CacheInner>>,
    max_bytes: usize,
    max_images: usize,
    /// Root directory to scan for `<uuid8>.fits` files when an in-memory
    /// lookup misses. The disk-fallback resolver matches by suffix and
    /// verifies via FITS `DOC_ID` (sidecar `id` as fallback authority).
    data_directory: PathBuf,
}

struct CacheInner {
    images: HashMap<String, Arc<CachedImage>>,
    /// Recency: front = LRU (next to evict), back = most recently used.
    order: VecDeque<String>,
    bytes: usize,
}

impl ImageCache {
    /// `max_mib`: eviction budget in mebibytes. `max_images`: hard cap on
    /// entries — a safety net against pathological large-image streams.
    /// `data_directory`: scanned by `resolve` / `resolve_document` on a
    /// cache miss to rehydrate from disk. Whichever budget trips first
    /// triggers eviction.
    pub fn new(max_mib: usize, max_images: usize, data_directory: PathBuf) -> Self {
        let max_bytes = max_mib.saturating_mul(1024 * 1024);
        debug!(
            max_mib = max_mib,
            max_images = max_images,
            data_directory = %data_directory.display(),
            "ImageCache constructed"
        );
        Self {
            inner: Arc::new(Mutex::new(CacheInner {
                images: HashMap::new(),
                order: VecDeque::new(),
                bytes: 0,
            })),
            max_bytes,
            max_images,
            data_directory,
        }
    }

    /// Insert an image under `document_id`. Replaces any existing entry for
    /// that id. Evicts LRU entries until both budgets are satisfied.
    pub fn insert(&self, document_id: &str, image: CachedImage) {
        let nbytes = image.nbytes();
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if let Some(prev) = inner.images.remove(document_id) {
            inner.bytes = inner.bytes.saturating_sub(prev.nbytes());
            inner.order.retain(|k| k != document_id);
        }

        inner.images.insert(document_id.to_owned(), Arc::new(image));
        inner.order.push_back(document_id.to_owned());
        inner.bytes = inner.bytes.saturating_add(nbytes);

        debug!(
            document_id = %document_id,
            nbytes = nbytes,
            cache_bytes = inner.bytes,
            cache_count = inner.images.len(),
            "ImageCache inserted"
        );

        self.evict_locked(&mut inner);
    }

    /// In-memory lookup only. Returns the cached image for `document_id`
    /// if present, marking it most-recently-used. `None` on miss — for
    /// disk fallback use [`resolve`](Self::resolve).
    #[must_use]
    pub fn get(&self, document_id: &str) -> Option<Arc<CachedImage>> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let image = inner.images.get(document_id).cloned()?;
        inner.order.retain(|k| k != document_id);
        inner.order.push_back(document_id.to_string());
        drop(inner);
        Some(image)
    }

    /// Resolve `document_id` to a cache entry, with on-disk rehydration
    /// on miss. On cache hit returns immediately. On miss scans
    /// `data_directory` for filenames whose suffix matches the
    /// document's UUID-8, verifies the candidate via the FITS `DOC_ID`
    /// header (sidecar `id` as fallback when the FITS is unreadable),
    /// reads both files, populates the cache as MRU, and returns the
    /// rehydrated entry. `None` when no candidate matches or the
    /// sidecar's `max_adu` is `null` (the cache cannot represent an
    /// image whose pixel type is unknown — callers fall back to
    /// `resolve_document` + direct FITS read for that rare case).
    pub async fn resolve(&self, document_id: &str) -> Option<Arc<CachedImage>> {
        if let Some(image) = self.get(document_id) {
            return Some(image);
        }
        debug!(document_id, "resolve: in-memory miss, scanning disk");
        let dir = self.data_directory.clone();
        let id = document_id.to_string();
        let image = tokio::task::spawn_blocking(move || disk_resolve_to_cached_image(&dir, &id))
            .await
            .ok()
            .flatten()?;
        self.insert(document_id, image);
        self.get(document_id)
    }

    /// Resolve `document_id` to its `ExposureDocument`, with on-disk
    /// rehydration on miss. Same disk-scan algorithm as [`resolve`] but
    /// returns the document even when the cache cannot hold the pixels
    /// (e.g. sidecar `max_adu == null`). Callers that need the raw FITS
    /// path use this to obtain `file_path`.
    pub async fn resolve_document(&self, document_id: &str) -> Option<ExposureDocument> {
        if let Some(image) = self.get(document_id) {
            let cloned = image.document.read().await.clone();
            return Some(cloned);
        }
        debug!(
            document_id,
            "resolve_document: in-memory miss, scanning disk"
        );
        let dir = self.data_directory.clone();
        let id = document_id.to_string();
        tokio::task::spawn_blocking(move || disk_resolve_document(&dir, &id))
            .await
            .ok()
            .flatten()
    }

    /// Resolve a FITS file path to its `ExposureDocument` by reading
    /// the sibling `.json` sidecar. Used by the `plate_solve` MCP
    /// tool's `image_path` mode so the late-solve workflow (rp
    /// captures frame N → starts capture N+1 → solves frame N) can
    /// update the original sidecar without needing the document id
    /// threaded through the call site. Returns `None` when the
    /// sidecar is missing, unreadable, doesn't parse as an
    /// `ExposureDocument`, or carries a `file_path` whose basename
    /// disagrees with the requested FITS — that last guard catches
    /// the cross-rig-mixed-sidecars case (operator hand-copies a
    /// sidecar from another rig's data directory; UUID-8 collision
    /// would otherwise drive `put_section`'s disk-fallback to write
    /// to the wrong file). Basename-only — full-path equality would
    /// false-positive when an operator archives a session to NAS.
    ///
    /// The path-to-sidecar mapping is `<base>.fits` → `<base>.json`;
    /// this mirrors how `mcp.rs:capture` writes the pair atomically
    /// (Phase 7 Step 1).
    pub async fn resolve_document_by_path(&self, fits_path: &str) -> Option<ExposureDocument> {
        let sidecar_path = ExposureDocument::sidecar_path_for(fits_path)?;
        let request_basename = Path::new(fits_path).file_name()?.to_owned();
        let doc = tokio::task::spawn_blocking(move || {
            ExposureDocument::read_sidecar_sync(&sidecar_path).ok()
        })
        .await
        .ok()
        .flatten()?;
        let doc_basename = Path::new(&doc.file_path).file_name();
        if doc_basename != Some(request_basename.as_os_str()) {
            debug!(
                requested = %fits_path,
                doc_file_path = %doc.file_path,
                "resolve_document_by_path: sidecar basename mismatch, declining"
            );
            return None;
        }
        Some(doc)
    }

    /// Write `value` into `sections[name]` on the document and persist
    /// the updated sidecar JSON atomically.
    ///
    /// **Cached entry path (the hot path).** The per-entry write lock
    /// is held across the sidecar write so concurrent updates
    /// serialize at the entry level. On sidecar-write failure the
    /// in-memory section is rolled back to its prior value, mirroring
    /// the old `DocumentStore::put_section` contract. The cache budget
    /// is updated only after a successful write.
    ///
    /// **Disk-fallback path.** When the cache entry is absent (post-
    /// eviction or a fresh `rp` restart), `put_section` rehydrates the
    /// document from its sidecar on disk, applies the section update,
    /// and writes back atomically. This is what makes the late-solve
    /// workflow's `image_path`-mode persistence durable across cache
    /// misses — the rare race between two concurrent fallback writers
    /// is benign because `write_sidecar_at` uses an atomic rename, so
    /// last writer wins with no torn state. The cache itself is left
    /// untouched on this path (no rehydration of pixels, no eviction
    /// budget shuffle); a subsequent `resolve(doc_id)` will pick up
    /// the new section the next time the document is hot.
    ///
    /// # Errors
    ///
    /// Returns the sidecar write's error on either path
    /// ([`ExposureDocument::write_sidecar`]); on the disk-fallback path
    /// additionally [`crate::error::RpError::Imaging`] if the document
    /// cannot be found on disk, its `file_path` is not a `.fits` path,
    /// or the blocking scan task fails to join.
    pub async fn put_section(
        &self,
        document_id: &str,
        name: &str,
        value: serde_json::Value,
    ) -> crate::error::Result<()> {
        if let Some(image) = self.get(document_id) {
            return self
                .put_section_cached(image, document_id, name, value)
                .await;
        }
        debug!(
            document_id, section = %name,
            "put_section: cache miss, attempting disk fallback"
        );
        self.put_section_disk_fallback(document_id, name, value)
            .await
    }

    async fn put_section_cached(
        &self,
        image: Arc<CachedImage>,
        document_id: &str,
        name: &str,
        value: serde_json::Value,
    ) -> crate::error::Result<()> {
        let mut doc = image.document.write().await;
        let prior = doc.sections.insert(name.to_string(), value);
        match doc.write_sidecar().await {
            Ok(()) => {
                let new_json_bytes = serde_json::to_vec(&*doc).map_or(0, |v| v.len());
                let old_json_bytes = image.json_nbytes.swap(new_json_bytes, Ordering::Relaxed);
                drop(doc);
                self.adjust_bytes(
                    i64::try_from(new_json_bytes)
                        .unwrap_or(i64::MAX)
                        .saturating_sub(i64::try_from(old_json_bytes).unwrap_or(i64::MAX)),
                );
                debug!(
                    document_id = %document_id,
                    section = %name,
                    new_json_bytes,
                    old_json_bytes,
                    "ImageCache put_section (cached)"
                );
                Ok(())
            }
            Err(e) => {
                match prior {
                    Some(v) => {
                        doc.sections.insert(name.to_string(), v);
                    }
                    None => {
                        doc.sections.remove(name);
                    }
                }
                drop(doc);
                Err(e)
            }
        }
    }

    async fn put_section_disk_fallback(
        &self,
        document_id: &str,
        name: &str,
        value: serde_json::Value,
    ) -> crate::error::Result<()> {
        // Same disk-scan path resolve_document uses on miss: locate
        // the sidecar by UUID-8 filename suffix + DOC_ID/sidecar id
        // verification. Returns the parsed document or None.
        let dir = self.data_directory.clone();
        let id = document_id.to_string();
        let doc_opt = tokio::task::spawn_blocking(move || disk_resolve_document(&dir, &id))
            .await
            .map_err(|e| {
                crate::error::RpError::Imaging(format!(
                    "disk-fallback put_section: join error: {e}"
                ))
            })?;
        let mut doc = doc_opt.ok_or_else(|| {
            crate::error::RpError::Imaging(format!("document not found: {document_id}"))
        })?;
        doc.sections.insert(name.to_string(), value);
        // Reuse the document's own `file_path` to derive the sidecar
        // path — the same convention `mcp.rs:capture` writes.
        let sidecar_path = ExposureDocument::sidecar_path_for(&doc.file_path).ok_or_else(|| {
            crate::error::RpError::Imaging(format!(
                "disk-fallback put_section: file_path is not a .fits path: {}",
                doc.file_path
            ))
        })?;
        doc.write_sidecar_at(&sidecar_path).await?;
        debug!(
            document_id = %document_id,
            section = %name,
            sidecar_path = %sidecar_path.display(),
            "ImageCache put_section (disk fallback)"
        );
        Ok(())
    }

    /// Apply a signed delta to the running cache `bytes` total under the
    /// cache mutex, then run eviction so the budget stays honored after
    /// document growth.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "eviction runs under the same lock acquisition as the budget update; the guard is handed to evict_locked"
    )]
    fn adjust_bytes(&self, delta: i64) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let magnitude = usize::try_from(delta.unsigned_abs()).unwrap_or(usize::MAX);
        if delta >= 0 {
            inner.bytes = inner.bytes.saturating_add(magnitude);
        } else {
            inner.bytes = inner.bytes.saturating_sub(magnitude);
        }
        self.evict_locked(&mut inner);
    }

    /// Number of entries currently in the cache.
    #[cfg(test)]
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .images
            .len()
    }

    #[cfg(test)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total bytes currently held.
    #[cfg(test)]
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .bytes
    }

    fn evict_locked(&self, inner: &mut CacheInner) {
        while inner.bytes > self.max_bytes || inner.images.len() > self.max_images {
            let Some(key) = inner.order.pop_front() else {
                break;
            };
            if let Some(evicted) = inner.images.remove(&key) {
                inner.bytes = inner.bytes.saturating_sub(evicted.nbytes());
                debug!(
                    document_id = %key,
                    cache_bytes = inner.bytes,
                    cache_count = inner.images.len(),
                    "ImageCache evicted"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Disk-fallback resolution helpers
// ---------------------------------------------------------------------------

/// Filename match: `<uuid8>.fits` (greenfield) or `<base>_<uuid8>.fits`
/// (future operator-template form). Must match exactly at the suffix; we
/// disambiguate by reading FITS `DOC_ID` afterwards, but the prefilter
/// keeps the candidate set small.
fn matches_uuid8_suffix(name: &str, uuid8: &str) -> bool {
    let needle = format!("{uuid8}.fits");
    name == needle || name.ends_with(&format!("_{needle}"))
}

/// Find candidate FITS files in `dir` whose name suffix matches the
/// document's UUID-8 prefilter.
fn find_candidates_by_suffix(dir: &Path, full_uuid: &str) -> Vec<PathBuf> {
    let Some(uuid8) = full_uuid.get(..8) else {
        return Vec::new();
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            debug!(
                ?dir,
                error = %e,
                "find_candidates_by_suffix: read_dir failed"
            );
            return Vec::new();
        }
    };
    let mut out: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches_uuid8_suffix(&name, uuid8) {
            out.push(entry.path());
        }
    }
    out.sort();
    debug!(
        ?dir,
        full_uuid,
        candidate_count = out.len(),
        "find_candidates_by_suffix"
    );
    out
}

/// Confirm that `fits_path` and its sidecar resolve to the requested
/// `full_uuid`. FITS `DOC_ID` is the preferred authority; sidecar `id`
/// is the fallback when the FITS is unreadable. Returns the sidecar
/// path on match.
fn confirm_candidate(fits_path: &Path, full_uuid: &str) -> Option<PathBuf> {
    if let Ok(Some(doc_id)) = super::fits::read_fits_doc_id(fits_path) {
        if doc_id == full_uuid {
            return ExposureDocument::sidecar_path_for(fits_path);
        }
        // FITS has a DOC_ID but it's a different doc — this candidate is
        // a confirmed ghost match, skip without trying the sidecar.
        return None;
    }
    // FITS unreadable or missing DOC_ID. Try the sidecar's id field.
    let sidecar = ExposureDocument::sidecar_path_for(fits_path)?;
    match ExposureDocument::read_sidecar_sync(&sidecar) {
        Ok(doc) if doc.id == full_uuid => Some(sidecar),
        _ => None,
    }
}

/// Disk-resolve to a full `CachedImage`. Returns `None` when the
/// sidecar's `max_adu` is `null` (the cache cannot represent it) or
/// when no candidate matches.
fn disk_resolve_to_cached_image(dir: &Path, full_uuid: &str) -> Option<CachedImage> {
    for fits_path in find_candidates_by_suffix(dir, full_uuid) {
        let Some(sidecar_path) = confirm_candidate(&fits_path, full_uuid) else {
            continue;
        };
        let doc = match ExposureDocument::read_sidecar_sync(&sidecar_path) {
            Ok(d) => d,
            Err(e) => {
                debug!(?sidecar_path, error = %e, "disk_resolve: sidecar parse failed");
                continue;
            }
        };
        let Some(max_adu) = doc.max_adu else {
            debug!(
                full_uuid,
                "disk_resolve: sidecar max_adu is null, declining cache insert"
            );
            return None;
        };
        let (pixels, width, height) = match super::fits::read_fits_pixels(&fits_path) {
            Ok(t) => t,
            Err(e) => {
                debug!(?fits_path, error = %e, "disk_resolve: FITS read failed");
                continue;
            }
        };
        // `CachedImage` reports these dimensions in the Alpaca
        // `ImageBytes` header, so they stop being buffer lengths here.
        // Checked before the narrowing pass below, which would otherwise
        // walk a frame this candidate is about to be dropped for.
        let (Ok(wire_w), Ok(wire_h)) = (u32::try_from(width), u32::try_from(height)) else {
            debug!(
                ?fits_path,
                width, height, "disk_resolve: frame too large for the ImageBytes header"
            );
            continue;
        };
        let cp = CachedPixels::from_i32_pixels(pixels, (width, height), max_adu)?;
        return Some(CachedImage::new(
            cp, wire_w, wire_h, fits_path, max_adu, doc,
        ));
    }
    None
}

/// Disk-resolve to just the `ExposureDocument`. Used when callers need
/// only the document — e.g. routes' `GET /api/documents/{id}`, or to
/// reach `file_path` for a FITS that the cache can't represent
/// (`max_adu == null`).
fn disk_resolve_document(dir: &Path, full_uuid: &str) -> Option<ExposureDocument> {
    for fits_path in find_candidates_by_suffix(dir, full_uuid) {
        let Some(sidecar_path) = confirm_candidate(&fits_path, full_uuid) else {
            continue;
        };
        match ExposureDocument::read_sidecar_sync(&sidecar_path) {
            Ok(doc) => return Some(doc),
            Err(e) => {
                debug!(?sidecar_path, error = %e, "disk_resolve_document: parse failed");
            }
        }
    }
    None
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
// Slicing a literal UUID down to its 8-char disk key. There is no
// `allow-string-slice-in-tests` knob, so the exemption is scoped here.
#[allow(clippy::string_slice)]
mod tests {
    use super::*;
    use ndarray::Array2;
    use serde_json::Map;

    /// Minimal dummy document. Tests that exercise byte-budget accounting
    /// pair this with `json_nbytes = 0` so they stay focused on pixel
    /// bytes and don't depend on serde formatting choices.
    fn dummy_document(id: &str) -> ExposureDocument {
        ExposureDocument {
            target: None,
            frame_type: None,
            id: id.to_string(),
            captured_at: "2026-04-30T00:00:00Z".to_string(),
            file_path: format!("/tmp/{id}.fits"),
            width: 0,
            height: 0,
            camera_id: None,
            duration: None,
            max_adu: None,
            cooler_setpoint_c: None,
            sensor_temperature_c: None,
            optics: None,
            sections: Map::new(),
        }
    }

    /// Pixel-only test fixture: builds a `CachedImage` with a dummy
    /// document but accounts zero JSON bytes, isolating byte-budget tests
    /// from the actual serialized doc size.
    fn u16_image(side: usize, fill: u16) -> CachedImage {
        let pixels = CachedPixels::U16(Array2::from_elem((side, side), fill));
        CachedImage {
            pixels,
            width: side as u32,
            height: side as u32,
            fits_path: PathBuf::from(format!("/tmp/{side}.fits")),
            max_adu: 65535,
            document: RwLock::new(dummy_document("doc")),
            json_nbytes: AtomicUsize::new(0),
        }
    }

    fn i32_image(side: usize, fill: i32) -> CachedImage {
        let pixels = CachedPixels::I32(Array2::from_elem((side, side), fill));
        CachedImage {
            pixels,
            width: side as u32,
            height: side as u32,
            fits_path: PathBuf::from(format!("/tmp/{side}.fits")),
            max_adu: 1 << 20,
            document: RwLock::new(dummy_document("doc")),
            json_nbytes: AtomicUsize::new(0),
        }
    }

    #[test]
    fn insert_and_get_round_trip() {
        let cache = ImageCache::new(100, 10, PathBuf::from("/nonexistent"));
        cache.insert("doc-1", u16_image(4, 42));

        let got = cache.get("doc-1").unwrap();
        assert_eq!(got.width, 4);
        assert_eq!(got.height, 4);
        assert_eq!(got.max_adu, 65535);
        match &got.pixels {
            CachedPixels::U16(arr) => assert_eq!(arr[[0, 0]], 42),
            CachedPixels::I32(_) => panic!("expected u16 variant"),
        }
    }

    #[test]
    fn miss_returns_none() {
        let cache = ImageCache::new(100, 10, PathBuf::from("/nonexistent"));
        assert!(cache.get("nope").is_none());
    }

    #[test]
    fn is_empty_tracks_population() {
        let cache = ImageCache::new(100, 10, PathBuf::from("/nonexistent"));
        assert!(cache.is_empty());
        cache.insert("doc-1", u16_image(4, 0));
        assert!(!cache.is_empty());
    }

    #[test]
    fn replacing_same_id_does_not_double_count_bytes() {
        let cache = ImageCache::new(100, 10, PathBuf::from("/nonexistent"));
        cache.insert("doc-1", u16_image(4, 1));
        let bytes_after_first = cache.bytes();
        cache.insert("doc-1", u16_image(4, 2));
        assert_eq!(cache.bytes(), bytes_after_first);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn evicts_when_image_count_exceeds_cap() {
        let cache = ImageCache::new(1024, 2, PathBuf::from("/nonexistent"));
        cache.insert("doc-1", u16_image(2, 1));
        cache.insert("doc-2", u16_image(2, 2));
        cache.insert("doc-3", u16_image(2, 3));
        assert_eq!(cache.len(), 2);
        assert!(cache.get("doc-1").is_none());
        assert!(cache.get("doc-2").is_some());
        assert!(cache.get("doc-3").is_some());
    }

    #[test]
    fn evicts_when_byte_budget_exceeded() {
        // Budget: 1 MiB. Each 1024x1024 u16 = 2 MiB → only one fits.
        let cache = ImageCache::new(1, 100, PathBuf::from("/nonexistent"));
        cache.insert("doc-1", u16_image(512, 1)); // 0.5 MiB
        cache.insert("doc-2", u16_image(512, 2)); // 0.5 MiB total 1 MiB
        assert_eq!(cache.len(), 2);
        cache.insert("doc-3", u16_image(512, 3)); // pushes over
        assert!(
            cache.get("doc-1").is_none(),
            "doc-1 should have been evicted"
        );
        assert!(cache.get("doc-2").is_some());
        assert!(cache.get("doc-3").is_some());
    }

    #[test]
    fn get_promotes_to_most_recently_used() {
        let cache = ImageCache::new(1024, 2, PathBuf::from("/nonexistent"));
        cache.insert("doc-1", u16_image(2, 1));
        cache.insert("doc-2", u16_image(2, 2));
        // Touch doc-1 to make it MRU.
        let _ = cache.get("doc-1");
        cache.insert("doc-3", u16_image(2, 3));
        // doc-2 should now be the LRU and evicted.
        assert!(cache.get("doc-2").is_none());
        assert!(cache.get("doc-1").is_some());
        assert!(cache.get("doc-3").is_some());
    }

    #[test]
    fn i32_variant_round_trips() {
        let cache = ImageCache::new(100, 10, PathBuf::from("/nonexistent"));
        cache.insert("doc-i", i32_image(4, 100_000));
        let got = cache.get("doc-i").unwrap();
        match &got.pixels {
            CachedPixels::I32(arr) => assert_eq!(arr[[0, 0]], 100_000),
            CachedPixels::U16(_) => panic!("expected i32 variant"),
        }
        assert_eq!(got.max_adu, 1 << 20);
    }

    #[test]
    fn nbytes_accounts_for_pixel_size() {
        let u16_img = u16_image(10, 0);
        let i32_img = i32_image(10, 0);
        assert_eq!(u16_img.pixels.nbytes(), 100 * 2);
        assert_eq!(i32_img.pixels.nbytes(), 100 * 4);
    }

    #[test]
    fn cached_image_new_includes_serialized_doc_in_nbytes() {
        // Pins the contract that `CachedImage::new` accounts for the
        // serialized document size. Step 4 will use this to keep the
        // running cache budget honest after `put_section` mutations.
        let pixels = CachedPixels::U16(Array2::from_elem((4, 4), 0u16));
        let pixel_bytes = pixels.nbytes();
        let doc = dummy_document("550e8400-e29b-41d4-a716-446655440000");
        let expected_json_bytes = serde_json::to_vec(&doc).unwrap().len();
        let img = CachedImage::new(pixels, 4, 4, PathBuf::from("/tmp/x.fits"), 65535, doc);
        assert_eq!(img.json_nbytes.load(Ordering::Relaxed), expected_json_bytes);
        assert_eq!(img.nbytes(), pixel_bytes + expected_json_bytes);
    }

    #[tokio::test]
    async fn cached_image_new_round_trips_document() {
        let doc = dummy_document("doc-1");
        let pixels = CachedPixels::U16(Array2::from_elem((2, 2), 0u16));
        let img = CachedImage::new(pixels, 2, 2, PathBuf::from("/tmp/x.fits"), 65535, doc);
        let read = img.document.read().await;
        assert_eq!(read.id, "doc-1");
    }

    // ---------------------------------------------------------------
    // Disk-fallback resolution tests (Step 5)
    // ---------------------------------------------------------------

    /// Write a complete FITS+sidecar pair into `dir` and return the full
    /// document UUID. The sidecar carries `max_adu = Some(65535)` by
    /// default so `disk_resolve` can pick the U16 cache variant.
    async fn write_disk_pair(
        dir: &Path,
        doc_uuid: &str,
        pixels: &[u16],
        width: usize,
        height: usize,
    ) -> String {
        let uuid8 = &doc_uuid[..8];
        let fits_path = dir.join(format!("{uuid8}.fits"));
        let sidecar_path = dir.join(format!("{uuid8}.json"));
        crate::persistence::write_fits_u16(&fits_path, pixels, width, height, doc_uuid)
            .await
            .unwrap();
        let mut doc = dummy_document(doc_uuid);
        doc.file_path = fits_path.to_string_lossy().into_owned();
        doc.width = u32::try_from(width).unwrap();
        doc.height = u32::try_from(height).unwrap();
        doc.max_adu = Some(65535);
        let body = serde_json::to_vec(&doc).unwrap();
        std::fs::write(&sidecar_path, body).unwrap();
        doc_uuid.to_string()
    }

    #[tokio::test]
    async fn resolve_hits_disk_after_eviction() {
        // Capture-equivalent: write FITS+sidecar to disk, no in-memory
        // entry. resolve() must rehydrate from disk and a follow-up
        // get() must be an in-memory hit (no second disk scan).
        let dir = tempfile::tempdir().unwrap();
        let doc_uuid = "11111111-1111-1111-1111-111111111111";
        write_disk_pair(dir.path(), doc_uuid, &[1u16, 2, 3, 4], 2, 2).await;
        let cache = ImageCache::new(64, 4, dir.path().to_path_buf());

        // Cold lookup: disk fallback fires.
        let image = cache.resolve(doc_uuid).await.expect("disk resolve");
        assert_eq!(image.width, 2);
        assert_eq!(image.height, 2);
        assert_eq!(image.max_adu, 65535);

        // Hot lookup: same entry, no disk roundtrip needed.
        let again = cache.get(doc_uuid).expect("in-memory hit after rehydrate");
        assert!(Arc::ptr_eq(&image, &again));
    }

    #[tokio::test]
    async fn resolve_returns_none_when_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ImageCache::new(64, 4, dir.path().to_path_buf());
        assert!(cache
            .resolve("00000000-0000-0000-0000-000000000000")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn resolve_handles_ghost_match() {
        // Two FITS+sidecar pairs whose first 8 hex chars are identical
        // but full UUIDs differ. The suffix prefilter returns both;
        // DOC_ID disambiguation must select the requested one.
        let dir = tempfile::tempdir().unwrap();
        let target_uuid = "deadbeef-1111-1111-1111-111111111111";
        let ghost_uuid = "deadbeef-2222-2222-2222-222222222222";
        // Both have the same uuid8 = "deadbeef", so file basename
        // collides. Place them in different subdirs to avoid the
        // basename collision while still sharing the suffix prefilter.
        // Actually: collision IS the point. We can't put both in the
        // same dir with the same basename; the design relies on the
        // greenfield assumption that no two captures share uuid8 in
        // the same data directory. Test the realistic ghost case
        // instead: a manually-renamed legacy file with `_<uuid8>.fits`
        // form sharing the suffix.
        let target_path = dir.path().join("deadbeef.fits");
        crate::persistence::write_fits_u16(&target_path, &[10u16, 20, 30, 40], 2, 2, target_uuid)
            .await
            .unwrap();
        let mut target_doc = dummy_document(target_uuid);
        target_doc.file_path = target_path.to_string_lossy().into_owned();
        target_doc.width = 2;
        target_doc.height = 2;
        target_doc.max_adu = Some(65535);
        std::fs::write(
            dir.path().join("deadbeef.json"),
            serde_json::to_vec(&target_doc).unwrap(),
        )
        .unwrap();

        // Ghost has the suffix `_deadbeef.fits` but a different DOC_ID.
        let ghost_path = dir.path().join("legacy_deadbeef.fits");
        crate::persistence::write_fits_u16(&ghost_path, &[99u16; 4], 2, 2, ghost_uuid)
            .await
            .unwrap();
        let mut ghost_doc = dummy_document(ghost_uuid);
        ghost_doc.file_path = ghost_path.to_string_lossy().into_owned();
        ghost_doc.width = 2;
        ghost_doc.height = 2;
        ghost_doc.max_adu = Some(65535);
        std::fs::write(
            dir.path().join("legacy_deadbeef.json"),
            serde_json::to_vec(&ghost_doc).unwrap(),
        )
        .unwrap();

        let cache = ImageCache::new(64, 4, dir.path().to_path_buf());
        let image = cache.resolve(target_uuid).await.expect("resolve target");
        // The cached pixel buffer should be the target's [10, 20, 30, 40]
        // (clamped to u16, max_adu=65535), not the ghost's 99s.
        match &image.pixels {
            CachedPixels::U16(arr) => {
                assert_eq!(
                    arr.as_slice().unwrap(),
                    &[10u16, 20, 30, 40],
                    "resolved to ghost instead of target"
                );
            }
            CachedPixels::I32(_) => panic!("expected U16 variant"),
        }
    }

    #[tokio::test]
    async fn resolve_skips_when_max_adu_unknown() {
        // Sidecar `max_adu: null` → resolve must return None even though
        // the document is on disk. Callers fall back to resolve_document
        // for the rare capture-time max_adu read failure.
        let dir = tempfile::tempdir().unwrap();
        let doc_uuid = "22222222-2222-2222-2222-222222222222";
        let uuid8 = &doc_uuid[..8];
        let fits_path = dir.path().join(format!("{uuid8}.fits"));
        crate::persistence::write_fits_u16(&fits_path, &[0u16; 4], 2, 2, doc_uuid)
            .await
            .unwrap();
        let mut doc = dummy_document(doc_uuid);
        doc.file_path = fits_path.to_string_lossy().into_owned();
        doc.width = 2;
        doc.height = 2;
        doc.max_adu = None;
        std::fs::write(
            dir.path().join(format!("{uuid8}.json")),
            serde_json::to_vec(&doc).unwrap(),
        )
        .unwrap();
        let cache = ImageCache::new(64, 4, dir.path().to_path_buf());

        assert!(cache.resolve(doc_uuid).await.is_none());
        // resolve_document still returns the doc — that's the escape
        // hatch for callers that need file_path for direct FITS reads.
        let resolved = cache
            .resolve_document(doc_uuid)
            .await
            .expect("resolve_document despite max_adu=null");
        assert_eq!(resolved.id, doc_uuid);
        assert!(resolved.max_adu.is_none());
    }

    #[tokio::test]
    async fn resolve_falls_back_to_sidecar_id_when_fits_corrupt() {
        // Pre-Phase-7 files lack DOC_ID; the resolver must fall back to
        // the sidecar's `id` field. Simulate by writing a raw rp_fits
        // HDU with no extra keywords (no DOC_ID stamping).
        let dir = tempfile::tempdir().unwrap();
        let doc_uuid = "33333333-3333-3333-3333-333333333333";
        let uuid8 = &doc_uuid[..8];
        let fits_path = dir.path().join(format!("{uuid8}.fits"));
        let mut file = std::fs::File::create(&fits_path).unwrap();
        rp_fits::writer::write_i32_image(&mut file, &[1i32, 2, 3, 4], 2, 2, &[]).unwrap();
        drop(file);
        let mut doc = dummy_document(doc_uuid);
        doc.file_path = fits_path.to_string_lossy().into_owned();
        doc.width = 2;
        doc.height = 2;
        doc.max_adu = Some(65535);
        std::fs::write(
            dir.path().join(format!("{uuid8}.json")),
            serde_json::to_vec(&doc).unwrap(),
        )
        .unwrap();

        let cache = ImageCache::new(64, 4, dir.path().to_path_buf());
        let image = cache
            .resolve(doc_uuid)
            .await
            .expect("resolve via sidecar id fallback");
        assert_eq!(image.width, 2);
    }

    // ---------------------------------------------------------------
    // put_section rollback (sidecar-write-failure path)
    // ---------------------------------------------------------------

    /// Force `write_sidecar` to fail by setting `file_path` to live inside
    /// a regular file. `create_dir_all(parent)` then errors with
    /// `NotADirectory` before any I/O touches disk, so the failure is
    /// deterministic and leaves no half-written state to clean up.
    fn unwriteable_doc(doc_id: &str, blocker: &Path) -> ExposureDocument {
        let mut doc = dummy_document(doc_id);
        doc.file_path = blocker.join("x.fits").to_string_lossy().into_owned();
        doc
    }

    #[tokio::test]
    async fn put_section_rolls_back_new_section_on_sidecar_write_failure() {
        // prior == None branch: section did not exist before the call;
        // on sidecar-write failure it must not exist after either.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();

        let pixels = CachedPixels::U16(Array2::from_elem((2, 2), 0u16));
        let image = CachedImage::new(
            pixels,
            2,
            2,
            PathBuf::from("/tmp/x.fits"),
            65535,
            unwriteable_doc("doc-1", &blocker),
        );
        let cache = ImageCache::new(64, 4, dir.path().to_path_buf());
        cache.insert("doc-1", image);

        cache
            .put_section("doc-1", "image_analysis", serde_json::json!({"hfr": 1.5}))
            .await
            .expect_err("sidecar write must fail for the test premise");

        let entry = cache.get("doc-1").unwrap();
        let after = entry.document.read().await;
        assert!(
            !after.sections.contains_key("image_analysis"),
            "new section must be rolled back when sidecar write fails"
        );
    }

    #[tokio::test]
    async fn put_section_restores_prior_value_on_sidecar_write_failure() {
        // prior == Some branch: existing section value must be restored
        // verbatim after a failed sidecar write.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();

        let prior = serde_json::json!({"hfr": 1.0, "marker": "prior"});
        let mut doc = unwriteable_doc("doc-1", &blocker);
        doc.sections
            .insert("image_analysis".to_string(), prior.clone());
        let pixels = CachedPixels::U16(Array2::from_elem((2, 2), 0u16));
        let image = CachedImage::new(pixels, 2, 2, PathBuf::from("/tmp/x.fits"), 65535, doc);
        let cache = ImageCache::new(64, 4, dir.path().to_path_buf());
        cache.insert("doc-1", image);

        cache
            .put_section("doc-1", "image_analysis", serde_json::json!({"hfr": 9.9}))
            .await
            .expect_err("sidecar write must fail for the test premise");

        let entry = cache.get("doc-1").unwrap();
        let after = entry.document.read().await;
        let stored = after
            .sections
            .get("image_analysis")
            .expect("prior section must still be present");
        assert_eq!(stored, &prior, "prior section value must be restored");
    }

    #[test]
    fn matches_uuid8_suffix_handles_greenfield_and_template_forms() {
        // Greenfield: <uuid8>.fits.
        assert!(matches_uuid8_suffix("550e8400.fits", "550e8400"));
        // Template form: <base>_<uuid8>.fits.
        assert!(matches_uuid8_suffix(
            "M31_L_5m_001_550e8400.fits",
            "550e8400"
        ));
        // Substring without underscore separator → reject.
        assert!(!matches_uuid8_suffix("xyz550e8400.fits", "550e8400"));
        // Different suffix → reject.
        assert!(!matches_uuid8_suffix("deadbeef.fits", "550e8400"));
        // Wrong extension → reject.
        assert!(!matches_uuid8_suffix("550e8400.json", "550e8400"));
    }

    // ---------------------------------------------------------------
    // resolve_document_by_path helpers (Phase 6c-2 Step 3)
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn resolve_document_by_path_returns_doc_for_known_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let doc_uuid = "550e8400-e29b-41d4-a716-446655440000";
        write_disk_pair(dir.path(), doc_uuid, &[0; 4], 2, 2).await;
        let cache = ImageCache::new(64, 4, dir.path().to_path_buf());
        let fits_path = dir
            .path()
            .join(format!("{}.fits", &doc_uuid[..8]))
            .to_string_lossy()
            .into_owned();
        let resolved = cache.resolve_document_by_path(&fits_path).await.unwrap();
        assert_eq!(resolved.id, doc_uuid);
    }

    #[tokio::test]
    async fn resolve_document_by_path_returns_none_for_external_fits() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ImageCache::new(64, 4, dir.path().to_path_buf());
        // Path doesn't exist on disk and has no UUID-8 suffix; the
        // late-solve workflow's "external FITS" case.
        let result = cache
            .resolve_document_by_path("/tmp/external-no-sidecar.fits")
            .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn resolve_document_by_path_returns_none_for_non_fits_path() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ImageCache::new(64, 4, dir.path().to_path_buf());
        let result = cache.resolve_document_by_path("/tmp/no-extension").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn resolve_document_by_path_rejects_sidecar_with_mismatched_basename() {
        // Cross-rig-mixed-sidecars guard: a sidecar whose `file_path`
        // basename disagrees with the requested FITS is declined,
        // even though the sidecar itself parses fine. Catches the
        // hand-copied-from-another-rig mistake that would otherwise
        // drive `put_section`'s disk-fallback to the wrong file.
        let dir = tempfile::tempdir().unwrap();
        let sidecar_path = dir.path().join("local.json");
        let mut foreign_doc = dummy_document("ffffffff-eeee-dddd-cccc-bbbbbbbbbbbb");
        // Sidecar claims it belongs to a *different* FITS file in
        // some other directory — exactly the cross-rig case.
        foreign_doc.file_path = "/data/other-rig/foreign.fits".to_string();
        std::fs::write(&sidecar_path, serde_json::to_vec(&foreign_doc).unwrap()).unwrap();

        let cache = ImageCache::new(64, 4, dir.path().to_path_buf());
        let local_fits = dir.path().join("local.fits").to_string_lossy().into_owned();
        let result = cache.resolve_document_by_path(&local_fits).await;
        assert!(
            result.is_none(),
            "sidecar with mismatched basename must be declined"
        );
    }

    #[tokio::test]
    async fn resolve_document_by_path_accepts_archived_session_directory() {
        // Archive workflow: operator copies the FITS+sidecar pair to a
        // new location (e.g. NAS). Basenames stay consistent; only
        // `doc.file_path` (which records the original capture path)
        // diverges. The guard accepts this — basename match suffices.
        let dir = tempfile::tempdir().unwrap();
        let basename = "abcd1234";
        let sidecar_path = dir.path().join(format!("{basename}.json"));
        let mut doc = dummy_document("550e8400-e29b-41d4-a716-446655440000");
        // Original capture path was elsewhere; the operator moved
        // the pair to the archive directory.
        doc.file_path = format!("/old/location/{basename}.fits");
        std::fs::write(&sidecar_path, serde_json::to_vec(&doc).unwrap()).unwrap();

        let cache = ImageCache::new(64, 4, dir.path().to_path_buf());
        let archived_fits = dir
            .path()
            .join(format!("{basename}.fits"))
            .to_string_lossy()
            .into_owned();
        let resolved = cache
            .resolve_document_by_path(&archived_fits)
            .await
            .expect("archive workflow must resolve despite path divergence");
        assert_eq!(resolved.id, "550e8400-e29b-41d4-a716-446655440000");
    }

    // ---------------------------------------------------------------
    // put_section disk-fallback (Phase 6c-2 review fix)
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn put_section_falls_back_to_disk_when_cache_misses() {
        let dir = tempfile::tempdir().unwrap();
        let doc_uuid = "550e8400-e29b-41d4-a716-446655440000";
        write_disk_pair(dir.path(), doc_uuid, &[0; 4], 2, 2).await;
        // Build a fresh cache that has *not* loaded the document.
        // This simulates the post-eviction / post-restart state the
        // late-solve workflow hits.
        let cache = ImageCache::new(64, 4, dir.path().to_path_buf());
        assert_eq!(cache.len(), 0);

        let payload = serde_json::json!({"ra_center": 10.6848, "solver": "test"});
        cache
            .put_section(doc_uuid, "wcs", payload.clone())
            .await
            .expect("put_section disk-fallback should succeed");

        // Cache was not rehydrated — disk fallback writes the
        // sidecar without inserting into the in-memory map.
        assert_eq!(cache.len(), 0, "disk fallback must not rehydrate the cache");

        // Verify the section landed on disk by reading the sidecar
        // back via the existing helper.
        let resolved = cache
            .resolve_document(doc_uuid)
            .await
            .expect("document still resolvable from disk");
        let stored = resolved
            .sections
            .get("wcs")
            .expect("wcs section must be present after disk-fallback write");
        assert_eq!(stored, &payload);
    }

    #[tokio::test]
    async fn put_section_disk_fallback_returns_not_found_for_unknown_id() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ImageCache::new(64, 4, dir.path().to_path_buf());
        let err = cache
            .put_section(
                "ffffffff-eeee-dddd-cccc-bbbbbbbbbbbb",
                "wcs",
                serde_json::json!({}),
            )
            .await
            .expect_err("unknown document id should error");
        assert!(err.to_string().contains("document not found"), "got: {err}");
    }
}

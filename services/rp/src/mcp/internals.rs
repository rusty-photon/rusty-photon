//! Cross-category helper methods on `McpHandler` that more than one
//! tool category needs, plus the small private types and free
//! functions they share.
//!
//! Kept in one file so changes that touch the capture/measure pipeline
//! land in one place.
//!
//! `pub(crate)` is the visibility we use for items called from sibling
//! `built_in/<category>.rs` files (e.g. `do_capture` is called from
//! both `built_in/camera.rs` and `built_in/auto_focus.rs`'s
//! `AutoFocusAdapter`). The `crate::mcp` module is private to the
//! crate, so `pub(crate)` does not widen the public API.

use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::camera::{CameraState, ImageArray};
use ascom_alpaca::api::Camera;
use tokio::time::Instant;
use tracing::debug;
use uuid::Uuid;

use rp_vocabulary::FrameType;

use crate::config::naming_template;
use crate::equipment::alpaca::retry_idempotent_read;
use crate::equipment::trains::TrainDeviceKind;
use crate::events::EventEnvelope;
use crate::imaging::{self, BackgroundStats, DetectionParams, Star};
use crate::persistence::{self, CachedImage, CachedPixels, ExposureDocument};

use super::handler::McpHandler;
use super::progress::{ProgressEmitter, PROGRESS_INTERVAL};

/// Backstop grace added to the requested exposure `duration` to bound
/// `do_capture`'s readout wait. An Alpaca camera can *fail* an exposure
/// — transition `CameraState` to `Error` and leave `ImageReady` false
/// (e.g. `sky-survey-camera` when its follow-mode mount read or survey
/// fetch times out) — or, more rarely, wedge in `Exposing`. The poll
/// loop treats `Error` as terminal; this grace caps the wait even when a
/// camera never reports either readiness or error. 120 s mirrors
/// `do_move_focuser_blocking`'s deadline and comfortably covers real
/// readout/download latency on top of the exposure itself.
const CAPTURE_READOUT_GRACE: Duration = Duration::from_mins(2);

/// Default per-camera readout + download estimate used to size the
/// predictive exposure deadline (§2.4) when `camera.readout_time_estimate`
/// is omitted. Conservative (slow side) so the advertised `predicted`
/// over-estimates rather than under-estimates — the Sentinel watchdog must
/// not flag a healthy-but-slow readout. A fast USB-3 CMOS reads out in well
/// under a second; this 15 s default is sized for an unconfigured slow
/// USB-2 CCD, and a real rig sets a tighter value per camera.
const DEFAULT_READOUT_TIME_ESTIMATE: Duration = Duration::from_secs(15);

/// Additive slack over the exposure `predicted` for the advertised
/// hard-ceiling `max` (§2.4): `max = duration + readout_time_estimate +
/// EXPOSURE_READOUT_HEADROOM`. Covers a slow USB-2 download tail beyond the
/// per-camera estimate. This sizes only the deadline carried on the
/// `exposure_started` envelope (which the Sentinel watchdog tracks) — rp's
/// own readout backstop is the separate, deliberately more generous
/// [`CAPTURE_READOUT_GRACE`]; the camera driver owns enforcement.
const EXPOSURE_READOUT_HEADROOM: Duration = Duration::from_secs(30);

/// Size the predictive exposure deadline (§2.4) for the `exposure_started`
/// envelope: `predicted = duration + readout_estimate`, `max = predicted +
/// EXPOSURE_READOUT_HEADROOM`. Pure millisecond math returning the
/// `(predicted_ms, max_ms)` envelope pair. Unlike the slew/park/focuser
/// helpers it takes no `&self` and never fails: the camera is already
/// resolved at the `do_capture` call site, there is no pre-op device read,
/// and rp does not enforce this deadline (so it returns no poll `Duration`).
/// Saturating arithmetic keeps an absurd operator-supplied `duration` from
/// overflowing rather than panicking.
pub(crate) fn exposure_deadlines(duration: Duration, readout_estimate: Duration) -> (u64, u64) {
    let predicted = duration.saturating_add(readout_estimate);
    let max = predicted.saturating_add(EXPOSURE_READOUT_HEADROOM);
    (
        u64::try_from(predicted.as_millis()).unwrap_or(u64::MAX),
        u64::try_from(max.as_millis()).unwrap_or(u64::MAX),
    )
}

/// Size the predictive `center_on_target` deadline (§2.5) for the
/// `centering_started` envelope. The Sentinel watchdog tracks only the
/// outer loop (each per-iteration `slew`/`capture` carries its own
/// deadline), so: `per_iter = capture_duration + solve_time_estimate +
/// slew_overhead_estimate`, `predicted = per_iter` (optimistic single-pass
/// convergence), `max = max_attempts × per_iter` (every attempt used).
/// Pure millisecond math returning the `(predicted_ms, max_ms)` envelope
/// pair; saturating arithmetic guards against overflow from an absurd
/// `duration` or `max_attempts`. Like [`exposure_deadlines`], this is
/// advisory only — rp does not enforce it (the inner ops do).
pub(crate) fn centering_deadlines(
    max_attempts: usize,
    capture_duration: Duration,
    solve_time_estimate: Duration,
    slew_overhead_estimate: Duration,
) -> (u64, u64) {
    let per_iter = capture_duration
        .saturating_add(solve_time_estimate)
        .saturating_add(slew_overhead_estimate);
    let predicted_ms = u64::try_from(per_iter.as_millis()).unwrap_or(u64::MAX);
    let max_ms = predicted_ms.saturating_mul(u64::try_from(max_attempts).unwrap_or(u64::MAX));
    (predicted_ms, max_ms)
}

/// Floor on the predictive slew deadline (§2.1 of the predictive-deadlines
/// plan). A short slew still gets at least this long before it's considered
/// overrun, covering fixed overheads that `distance / rate` ignores:
/// acceleration ramps and controller/`IsSlewing` latency. The binding
/// constraint is the `OmniSim` BDD simulator — it slews at 20°/s with a fixed
/// deceleration tail, so a from-rest slew (its physical axes reset to a
/// startup position each scenario, while `sync_mount` only moves the
/// *reported* coordinates) takes up to ~12 s regardless of the small
/// reported distance rp sizes the deadline from. A real mount's tiny slew
/// is far quicker, so this floor is slack in production. 30 s is ~2.5×
/// `OmniSim`'s ~12 s worst case — margin for a contended CI runner dropping
/// timer ticks (the goto-slew advances a fixed angle per tick, so a stalled
/// timer stretches wall-clock time) — while still surfacing a wedged slew
/// ~10× sooner than the prior hardcoded 300 s ceiling, and well before
/// rmcp's 300 s session keep-alive (the swallowed-hang trigger this plan
/// fixes).
const MIN_SLEW_DEADLINE: Duration = Duration::from_secs(30);

/// Slew deadline used when the predicted deadline can't be computed — the
/// mount isn't resolvable yet, or the pre-slew pointing read failed. A
/// prediction is an optimization, not a precondition for slewing, so the
/// deadline degrades to the historical 300 s ceiling rather than failing
/// the slew.
const SLEW_DEADLINE_FALLBACK: Duration = Duration::from_mins(5);

/// Worst-case axis traverse used to size the park deadline (§2.2). The
/// generic Alpaca `Telescope` trait exposes no park-position getter, so rp
/// cannot compute a great-circle distance to the park position the way
/// `slew` does. 180° is the maximum angular separation between any two
/// points on the sphere — the honest upper bound on how far park can
/// traverse without reading the park coordinates.
const PARK_WORST_CASE_TRAVERSE_DEG: f64 = 180.0;

/// Headroom multiplier over the worst-case park `predicted`. Smaller than
/// slew's ×3 (which sits over a *measured* small distance): park's
/// `predicted` is already a worst-case 180° traverse, so ×3 would re-approach
/// the old 300 s ceiling and defeat the point.
const PARK_DEADLINE_HEADROOM: f64 = 2.0;

/// Floor on the predictive park deadline. More generous than
/// [`MIN_SLEW_DEADLINE`] — park traverses to a fixed mechanical position
/// that can be a long way off, and `OmniSim`'s BDD park is a from-rest
/// physical traverse.
const MIN_PARK_DEADLINE: Duration = Duration::from_mins(1);

/// Park deadline used when no mount is configured (the only case in which
/// the park deadline can't be sized). Park would fail immediately without a
/// mount anyway; the fallback keeps the poll loop bounded for symmetry with
/// the slew path.
const PARK_DEADLINE_FALLBACK: Duration = Duration::from_mins(5);

/// Headroom multiplier over the focuser `predicted` (§2.3): `max =
/// max(predicted × 2, MIN_FOCUSER_DEADLINE)`.
const FOCUSER_DEADLINE_HEADROOM: f64 = 2.0;

/// Floor on the predictive `move_focuser` deadline — a tiny move still gets
/// at least this long, covering fixed controller/`IsMoving` latency.
const MIN_FOCUSER_DEADLINE: Duration = Duration::from_secs(5);

/// Move-focuser deadline used when the predicted deadline can't be computed
/// — the focuser isn't resolvable, or the pre-move position read failed. A
/// prediction is an optimization, not a precondition for moving, so the
/// deadline degrades to the historical 120 s ceiling rather than failing
/// the move.
const FOCUSER_DEADLINE_FALLBACK: Duration = Duration::from_mins(2);

// ---------------------------------------------------------------------------
// Private helper types shared across imaging tool bodies. All
// `pub(crate)` so individual category files can construct them.
// ---------------------------------------------------------------------------

/// `MeasureBasicParams` after schema-level optionals are validated by the
/// tool body. Pure data, no `Option`s — passed to the imaging composer.
pub(crate) struct ResolvedParams {
    pub(crate) threshold_sigma: f64,
    pub(crate) min_area: usize,
    pub(crate) max_area: usize,
}

/// `EstimateBackgroundParams` after sign/range validation. Same pattern as
/// `ResolvedParams`: schema-level optionals, validated in the tool body.
pub(crate) struct ResolvedClipParams {
    pub(crate) k: f64,
    pub(crate) max_iters: usize,
}

/// Background stats paired with the input pixel area (rows × cols). The
/// kernel's `BackgroundStats.n_pixels` is the *surviving* count after
/// sigma-clipping; `total_pixels` is what we report as `pixel_count` in
/// the tool's JSON contract — consistent with `measure_basic`.
pub(crate) struct BackgroundOutcome {
    pub(crate) stats: BackgroundStats,
    pub(crate) total_pixels: u64,
}

/// `DetectStarsParams` after schema-level optionals are validated by the
/// tool body. Pure data, no `Option`s — passed to the imaging composer.
pub(crate) struct ResolvedDetectParams {
    pub(crate) threshold_sigma: f64,
    pub(crate) min_area: usize,
    pub(crate) max_area: usize,
}

/// Detection outcome: the star list paired with the background stats used
/// to set the threshold. The tool's JSON contract surfaces both.
pub(crate) struct DetectStarsOutcome {
    pub(crate) stars: Vec<Star>,
    pub(crate) background: BackgroundStats,
}

/// `MeasureStarsParams` after schema-level optionals are validated by the
/// tool body.
pub(crate) struct ResolvedMeasureStarsParams {
    pub(crate) threshold_sigma: f64,
    pub(crate) min_area: usize,
    pub(crate) max_area: usize,
    pub(crate) stamp_half_size: usize,
}

/// Counts existing `.fits` frames in `dir` whose filename (parsed via
/// `file_template`) shares this frame's `(filter, binning, exposure_duration)`
/// sub-spec, and returns count + 1 — the `{frame_number}` value for a
/// new frame in that sub-spec. Nothing is stored (rp-targets.md §
/// Progress derivation): a plain `read_dir` scan per capture, scoped
/// to `dir` (the frame's own rendered directory, e.g. one target's one
/// observing night) rather than the whole data directory. `Ok(1)`
/// when `dir` doesn't exist yet — this sub-spec's first frame here.
/// Filters out `.json` sidecars explicitly: they share a filename stem
/// with their FITS file, so counting both would double-count.
/// The JSON sidecar carries fixed-width dimensions, so this is where
/// the frame geometry stops being a buffer length (the FITS writer
/// takes the lengths directly).
fn sidecar_dims(width: usize, height: usize) -> std::result::Result<(u32, u32), String> {
    match (u32::try_from(width), u32::try_from(height)) {
        (Ok(w), Ok(h)) => Ok((w, h)),
        _ => Err(format!(
            "captured frame {width}x{height} is too large to record in a sidecar"
        )),
    }
}

/// A fresh exposure-document id plus its 8-char UUID prefix — the
/// on-disk reverse-lookup key used by the cache's disk-fallback
/// resolution (`rp.md` Persistence). Taken from `time_low` rather than
/// by slicing the canonical form: it is the same eight hex digits the
/// canonical form starts with, but the width is guaranteed by the
/// format rather than by a bounds check that could quietly fall back
/// to a different naming scheme.
fn new_document_ids() -> (String, String) {
    let document_uuid = Uuid::new_v4();
    let document_id = document_uuid.to_string();
    let uuid8 = format!("{:08x}", document_uuid.as_fields().0);
    (document_id, uuid8)
}

/// The per-exposure snapshot of connect-time invariants, copied out of
/// the equipment-registry borrow so it need not outlive any await.
struct CaptureSnapshot {
    cam: Arc<dyn Camera>,
    focal_length_mm: Option<f64>,
    readout_time_estimate: Duration,
    cached_max_adu: Option<u32>,
    cached_optics: (Option<f64>, Option<f64>, Option<u32>, Option<u32>),
}

/// Dispatch on `max_adu`, collecting pixels directly into the
/// narrowest type each path needs, writing the FITS file, and reusing
/// the same buffer for the cache insert. `None` when the cache insert
/// is skipped (unknown `max_adu`).
async fn write_pixels(
    image_path: &str,
    image_array: ImageArray,
    shape: (usize, usize),
    document_id: &str,
    captured_max_adu: Option<u32>,
) -> std::result::Result<Option<CachedPixels>, String> {
    let (width, height) = shape;
    match captured_max_adu {
        Some(max_adu) if u16::try_from(max_adu).is_ok() => {
            let max_adu_i32 = max_adu.cast_signed();
            // Clamped into [0, max_adu] and the guard proved
            // max_adu fits u16, so the conversion cannot fail.
            let u16_pixels: Vec<u16> = image_array
                .iter()
                .map(|&p| u16::try_from(p.clamp(0, max_adu_i32)).unwrap_or(u16::MAX))
                .collect();
            drop(image_array);
            persistence::write_fits_u16(image_path, &u16_pixels, width, height, document_id)
                .await
                .map_err(|e| format!("failed to write FITS file: {e}"))?;
            Ok(CachedPixels::from_u16_pixels(u16_pixels, shape))
        }
        _ => {
            let i32_pixels: Vec<i32> = image_array.iter().copied().collect();
            drop(image_array);
            persistence::write_fits_i32(image_path, &i32_pixels, width, height, document_id)
                .await
                .map_err(|e| format!("failed to write FITS file: {e}"))?;
            Ok(captured_max_adu.and_then(|m| CachedPixels::from_i32_pixels(i32_pixels, shape, m)))
        }
    }
}

/// The per-exposure inputs `render_templated_path` needs beyond the
/// camera handle and target slug.
struct TemplateRenderCtx<'a> {
    camera_id: &'a str,
    frame_type: FrameType,
    duration: Duration,
    captured_at: chrono::DateTime<chrono::Utc>,
    sensor_temperature_c: Option<f64>,
    uuid8: &'a str,
}

/// Optical geometry for the sidecar's `optics` block. Combines the
/// operator-supplied focal length with the cached pixel-size and
/// sensor-dimension reads from `CameraEntry`. Any missing piece
/// (focal length not configured, connect-time read failed) drops
/// the whole block — see `docs/services/rp.md` §"Core Fields".
fn derive_optics(
    camera_id: &str,
    focal_length_mm: Option<f64>,
    cached_optics: (Option<f64>, Option<f64>, Option<u32>, Option<u32>),
) -> Option<persistence::Optics> {
    focal_length_mm.map_or_else(
        || {
            debug!(
                camera_id,
                "focal_length_mm not configured; omitting optics block"
            );
            None
        },
        |focal_length_mm| match cached_optics {
            (Some(px), Some(py), Some(sw), Some(sh)) => {
                let derived =
                    persistence::Optics::from_camera_geometry(persistence::CameraGeometry {
                        focal_length_mm,
                        pixel_size_x_um: px,
                        pixel_size_y_um: py,
                        sensor_width_px: sw,
                        sensor_height_px: sh,
                    });
                if derived.is_none() {
                    // All cached values are present but the
                    // derivation declined — typically a non-
                    // positive or wild-magnitude reading that
                    // would have overflowed the derived pixel
                    // scale / FOV. Surface enough to diagnose
                    // bad camera state or a misconfigured focal
                    // length.
                    debug!(
                        camera_id,
                        focal_length_mm,
                        pixel_size_x_um = px,
                        pixel_size_y_um = py,
                        sensor_width_px = sw,
                        sensor_height_px = sh,
                        "optics derivation declined; omitting block"
                    );
                }
                derived
            }
            _ => None,
        },
    )
}

async fn next_frame_number(
    file_template: &naming_template::CompiledTemplate,
    dir: &std::path::Path,
    filter: &str,
    binning: rp_targets::Binning,
    exposure_duration: Duration,
) -> std::result::Result<u32, String> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(1),
        Err(e) => {
            return Err(format!(
                "capture: failed to scan '{}': {}",
                dir.display(),
                e
            ))
        }
    };
    let mut count: u32 = 0;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| format!("capture: failed to scan '{}': {}", dir.display(), e))?
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("fits") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(parsed) = file_template.parse(stem) else {
            continue;
        };
        if parsed.filter.as_deref() == Some(filter)
            && parsed.binning == Some(binning)
            && parsed.exposure_duration == Some(exposure_duration)
        {
            count = count.saturating_add(1);
        }
    }
    Ok(count.saturating_add(1))
}

// ---------------------------------------------------------------------------
// `McpHandler` helper-method impl. Methods are `pub(crate)` so they're
// callable from sibling category files.
// ---------------------------------------------------------------------------

impl McpHandler {
    pub(crate) async fn stats_via_document(
        &self,
        doc_id: &str,
    ) -> crate::error::Result<imaging::ImageStats> {
        if let Some(cached) = self.image_cache.resolve(doc_id).await {
            return crate::dispatch_pixels!(&cached.pixels, |arr| stats_outcome(&arr));
        }

        debug!(document_id = %doc_id, "image cache miss, falling back to FITS");
        let doc = self
            .image_cache
            .resolve_document(doc_id)
            .await
            .ok_or_else(|| {
                crate::error::RpError::Imaging(format!("document not found: {doc_id}"))
            })?;
        self.stats_via_path(&doc.file_path).await
    }

    pub(crate) async fn stats_via_path(
        &self,
        path: &str,
    ) -> crate::error::Result<imaging::ImageStats> {
        let path_owned = path.to_string();
        tokio::task::spawn_blocking(move || {
            let (mut pixels, _w, _h) = persistence::read_fits_pixels(&path_owned)?;
            imaging::compute_stats(&mut pixels)
                .ok_or_else(|| crate::error::RpError::Imaging("image has no pixels".into()))
        })
        .await
        .map_err(|e| crate::error::RpError::Imaging(format!("task join error: {e}")))?
    }

    pub(crate) async fn measure_via_document(
        &self,
        doc_id: &str,
        params: &ResolvedParams,
    ) -> crate::error::Result<imaging::MeasureBasicResult> {
        if let Some(cached) = self.image_cache.resolve(doc_id).await {
            let max_adu = Some(cached.max_adu);
            return crate::dispatch_pixels!(&cached.pixels, |arr| imaging::measure_basic(
                &arr,
                params.threshold_sigma,
                params.min_area,
                params.max_area,
                max_adu,
            ));
        }

        debug!(document_id = %doc_id, "image cache miss, falling back to FITS");
        let doc = self
            .image_cache
            .resolve_document(doc_id)
            .await
            .ok_or_else(|| {
                crate::error::RpError::Imaging(format!("document not found: {doc_id}"))
            })?;
        // No camera context here, so we can't reliably know max_adu — pass None
        // (saturation flagging is best-effort; not a correctness issue).
        self.measure_via_path(&doc.file_path, params).await
    }

    pub(crate) async fn measure_via_path(
        &self,
        path: &str,
        params: &ResolvedParams,
    ) -> crate::error::Result<imaging::MeasureBasicResult> {
        let path_owned = path.to_string();
        let threshold = params.threshold_sigma;
        let min_a = params.min_area;
        let max_a = params.max_area;
        tokio::task::spawn_blocking(move || {
            let (pixels, width, height) = persistence::read_fits_pixels(&path_owned)?;
            let arr = ndarray::Array2::from_shape_vec((width, height), pixels)
                .map_err(|e| crate::error::RpError::Imaging(format!("FITS shape mismatch: {e}")))?;
            imaging::measure_basic(&arr.view(), threshold, min_a, max_a, None)
        })
        .await
        .map_err(|e| crate::error::RpError::Imaging(format!("task join error: {e}")))?
    }

    pub(crate) async fn estimate_via_document(
        &self,
        doc_id: &str,
        params: &ResolvedClipParams,
    ) -> crate::error::Result<BackgroundOutcome> {
        if let Some(cached) = self.image_cache.resolve(doc_id).await {
            return crate::dispatch_pixels!(&cached.pixels, |arr| clip_outcome(&arr, params));
        }

        debug!(document_id = %doc_id, "image cache miss, falling back to FITS");
        let doc = self
            .image_cache
            .resolve_document(doc_id)
            .await
            .ok_or_else(|| {
                crate::error::RpError::Imaging(format!("document not found: {doc_id}"))
            })?;
        self.estimate_via_path(&doc.file_path, params).await
    }

    pub(crate) async fn estimate_via_path(
        &self,
        path: &str,
        params: &ResolvedClipParams,
    ) -> crate::error::Result<BackgroundOutcome> {
        let path_owned = path.to_string();
        let k = params.k;
        let max_iters = params.max_iters;
        tokio::task::spawn_blocking(move || {
            let (pixels, width, height) = persistence::read_fits_pixels(&path_owned)?;
            let arr = ndarray::Array2::from_shape_vec((width, height), pixels)
                .map_err(|e| crate::error::RpError::Imaging(format!("FITS shape mismatch: {e}")))?;
            clip_outcome(&arr.view(), &ResolvedClipParams { k, max_iters })
        })
        .await
        .map_err(|e| crate::error::RpError::Imaging(format!("task join error: {e}")))?
    }

    pub(crate) async fn detect_via_document(
        &self,
        doc_id: &str,
        params: &ResolvedDetectParams,
    ) -> crate::error::Result<DetectStarsOutcome> {
        if let Some(cached) = self.image_cache.resolve(doc_id).await {
            let max_adu = Some(cached.max_adu);
            return crate::dispatch_pixels!(&cached.pixels, |arr| detect_outcome(
                &arr, params, max_adu
            ));
        }

        debug!(document_id = %doc_id, "image cache miss, falling back to FITS");
        let doc = self
            .image_cache
            .resolve_document(doc_id)
            .await
            .ok_or_else(|| {
                crate::error::RpError::Imaging(format!("document not found: {doc_id}"))
            })?;
        // No camera context here — pass max_adu = None (matches measure_basic).
        self.detect_via_path(&doc.file_path, params).await
    }

    pub(crate) async fn detect_via_path(
        &self,
        path: &str,
        params: &ResolvedDetectParams,
    ) -> crate::error::Result<DetectStarsOutcome> {
        let path_owned = path.to_string();
        let resolved = ResolvedDetectParams {
            threshold_sigma: params.threshold_sigma,
            min_area: params.min_area,
            max_area: params.max_area,
        };
        tokio::task::spawn_blocking(move || {
            let (pixels, width, height) = persistence::read_fits_pixels(&path_owned)?;
            let arr = ndarray::Array2::from_shape_vec((width, height), pixels)
                .map_err(|e| crate::error::RpError::Imaging(format!("FITS shape mismatch: {e}")))?;
            detect_outcome(&arr.view(), &resolved, None)
        })
        .await
        .map_err(|e| crate::error::RpError::Imaging(format!("task join error: {e}")))?
    }

    pub(crate) async fn measure_stars_via_document(
        &self,
        doc_id: &str,
        params: &ResolvedMeasureStarsParams,
    ) -> crate::error::Result<imaging::MeasureStarsResult> {
        if let Some(cached) = self.image_cache.resolve(doc_id).await {
            let max_adu = Some(cached.max_adu);
            return crate::dispatch_pixels!(&cached.pixels, |arr| imaging::measure_stars(
                &arr,
                params.threshold_sigma,
                params.min_area,
                params.max_area,
                max_adu,
                params.stamp_half_size,
            ));
        }

        debug!(document_id = %doc_id, "image cache miss, falling back to FITS");
        let doc = self
            .image_cache
            .resolve_document(doc_id)
            .await
            .ok_or_else(|| {
                crate::error::RpError::Imaging(format!("document not found: {doc_id}"))
            })?;
        self.measure_stars_via_path(&doc.file_path, params).await
    }

    pub(crate) async fn measure_stars_via_path(
        &self,
        path: &str,
        params: &ResolvedMeasureStarsParams,
    ) -> crate::error::Result<imaging::MeasureStarsResult> {
        let path_owned = path.to_string();
        let threshold = params.threshold_sigma;
        let min_a = params.min_area;
        let max_a = params.max_area;
        let stamp = params.stamp_half_size;
        tokio::task::spawn_blocking(move || {
            let (pixels, width, height) = persistence::read_fits_pixels(&path_owned)?;
            let arr = ndarray::Array2::from_shape_vec((width, height), pixels)
                .map_err(|e| crate::error::RpError::Imaging(format!("FITS shape mismatch: {e}")))?;
            imaging::measure_stars(&arr.view(), threshold, min_a, max_a, None, stamp)
        })
        .await
        .map_err(|e| crate::error::RpError::Imaging(format!("task join error: {e}")))?
    }

    pub(crate) async fn snr_via_document(
        &self,
        doc_id: &str,
        params: &ResolvedDetectParams,
    ) -> crate::error::Result<imaging::SnrResult> {
        if let Some(cached) = self.image_cache.resolve(doc_id).await {
            let max_adu = Some(cached.max_adu);
            return crate::dispatch_pixels!(&cached.pixels, |arr| imaging::compute_snr(
                &arr,
                params.threshold_sigma,
                params.min_area,
                params.max_area,
                max_adu,
            ));
        }

        debug!(document_id = %doc_id, "image cache miss, falling back to FITS");
        let doc = self
            .image_cache
            .resolve_document(doc_id)
            .await
            .ok_or_else(|| {
                crate::error::RpError::Imaging(format!("document not found: {doc_id}"))
            })?;
        self.snr_via_path(&doc.file_path, params).await
    }

    pub(crate) async fn snr_via_path(
        &self,
        path: &str,
        params: &ResolvedDetectParams,
    ) -> crate::error::Result<imaging::SnrResult> {
        let path_owned = path.to_string();
        let threshold = params.threshold_sigma;
        let min_a = params.min_area;
        let max_a = params.max_area;
        tokio::task::spawn_blocking(move || {
            let (pixels, width, height) = persistence::read_fits_pixels(&path_owned)?;
            let arr = ndarray::Array2::from_shape_vec((width, height), pixels)
                .map_err(|e| crate::error::RpError::Imaging(format!("FITS shape mismatch: {e}")))?;
            imaging::compute_snr(&arr.view(), threshold, min_a, max_a, None)
        })
        .await
        .map_err(|e| crate::error::RpError::Imaging(format!("task join error: {e}")))?
    }

    /// Persist the document and (on success) populate the image cache.
    ///
    /// Sidecar failure contract: if `write_sidecar` fails the cache insert
    /// is skipped, a `document_persistence_failed` event is emitted, and
    /// the function returns. The FITS file remains on disk; the
    /// `document_id` is unreachable via cache or disk fallback (no
    /// sidecar) until callers fall back to the FITS path directly. See
    /// `docs/services/rp.md` → Capture Tool Details → Sidecar failure
    /// contract.
    pub(crate) async fn persist_capture_artifact(
        &self,
        doc: ExposureDocument,
        cached_pixels: Option<CachedPixels>,
        captured_max_adu: Option<u32>,
    ) {
        let document_id = doc.id.clone();
        let image_path = doc.file_path.clone();
        let width = doc.width;
        let height = doc.height;

        let document_persisted = match doc.write_sidecar().await {
            Ok(()) => true,
            Err(e) => {
                debug!(error = %e, "sidecar write failed, skipping cache insert");
                self.event_bus.emit(
                    "document_persistence_failed",
                    serde_json::json!({
                        "document_id": document_id,
                        "file_path": image_path,
                        "error": e.to_string(),
                    }),
                );
                false
            }
        };

        if document_persisted {
            if let (Some(max_adu), Some(cp)) = (captured_max_adu, cached_pixels) {
                self.image_cache.insert(
                    &document_id,
                    CachedImage::new(
                        cp,
                        width,
                        height,
                        std::path::PathBuf::from(&image_path),
                        max_adu,
                        doc,
                    ),
                );
            }
        }
    }

    /// Run the full capture pipeline against the named camera and return
    /// `(image_path, document_id)`. Shared body of the `capture` MCP tool
    /// and the `auto_focus` compound tool's per-step capture call —
    /// both want the same exposure / FITS-write / cache-insert / event
    /// flow.
    ///
    /// When `progress` is `Some`, the poll loop emits
    /// `notifications/progress` every [`PROGRESS_INTERVAL`] so rmcp's
    /// 300 s session keep-alive cannot fire during a legitimate
    /// long exposure (`duration` plus `CAPTURE_READOUT_GRACE`). The
    /// emitted `progress` is the elapsed fraction of the total
    /// `duration + CAPTURE_READOUT_GRACE` budget; messages cycle
    /// `"exposing"` → `"reading_out"` once `image_ready` flips true.
    /// `None` (unit tests, MCP clients that omitted `progressToken`)
    /// makes the emission a no-op.
    pub(crate) async fn do_capture(
        &self,
        camera_id: &str,
        duration: Duration,
        target: Option<&str>,
        frame_type: Option<FrameType>,
        progress: Option<&dyn ProgressEmitter>,
    ) -> std::result::Result<(String, String), String> {
        let CaptureSnapshot {
            cam,
            focal_length_mm,
            readout_time_estimate,
            cached_max_adu,
            cached_optics,
        } = self.capture_snapshot(camera_id)?;

        // Imaging-train exposures contend with mount motion (rp.md
        // § Mount Motion Gate): hold the gate shared for the whole
        // pipeline, so a pending slew/dither delays this exposure's
        // start rather than trailing its stars. Un-trained and
        // guiding-train cameras bypass the gate — trains are
        // enrichment, not a gate. `exposure_started` below is emitted
        // only after the acquire, keeping its deadline honest.
        let _motion_permit = if self.trains.camera_in_imaging_train(camera_id) {
            Some(self.motion_gate.shared().await)
        } else {
            None
        };

        let (document_id, uuid8) = new_document_ids();
        let mut image_path = format!("{}/{}.fits", self.session_config.data_directory, uuid8);

        let operation_id = Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now();
        self.emit_exposure_started(
            &operation_id,
            started_at,
            camera_id,
            duration,
            readout_time_estimate,
        );

        // Run the exposure body (start → poll → download → write FITS →
        // persist) inside one future so the public method emits exactly
        // one of `exposure_complete` / `exposure_failed` to mirror the
        // `exposure_started` above, under a shared `operation_id`. `?`
        // and early `return Err` inside resolve to this block's Result.
        let capture_result: std::result::Result<(), String> = async {
            cam.start_exposure(duration, true)
                .await
                .map_err(|e| format!("failed to start exposure: {e}"))?;

            Self::wait_for_image_ready(&cam, duration, progress).await?;

            let (captured_at, cooler_setpoint_c, sensor_temperature_c) =
                self.capture_conditions(camera_id, &cam).await;

            // Decision 11 (rp.md § Capture Tool Details): `frame_type`
            // stamps the document's `target`/`frame_type` fields.
            // `None` leaves both unset. The *templated path* is a
            // separate switch — `session.file_naming_pattern` — so a rig
            // with no pattern configured still records what a frame is
            // and what it is of, it just keeps the flat
            // `<doc_uuid_8>.fits` name (and, having nothing on disk to
            // attribute, derives no progress; rp.md § Progress
            // derivation).
            let mut exposure_target: Option<persistence::ExposureTarget> = None;
            let mut resolved_frame_type: Option<FrameType> = None;
            if let Some(frame_type) = frame_type {
                let (target_field, target_slug) =
                    self.resolve_capture_target(target, frame_type).await?;
                exposure_target = Some(target_field);
                resolved_frame_type = Some(frame_type);

                let ctx = TemplateRenderCtx {
                    camera_id,
                    frame_type,
                    duration,
                    captured_at,
                    sensor_temperature_c,
                    uuid8: &uuid8,
                };
                if let Some(rendered) = self.render_templated_path(&cam, target_slug, ctx).await? {
                    image_path = rendered;
                }
            }

            let image_array = cam
                .image_array()
                .await
                .map_err(|e| format!("failed to download image array: {e}"))?;

            let (width, height, _planes) = image_array.dim();
            let (doc_width, doc_height) = sidecar_dims(width, height)?;

            // `captured_max_adu` decides whether we need a u16 or i32 buffer,
            // so it is consulted *before* collecting pixels to let us collect
            // straight into the destination type and avoid the wasted i32→u16
            // round trip.
            //
            // max_adu feeds three consumers: on-disk FITS bit-depth, cache
            // variant, and the exposure document's `max_adu` field
            // (sidecar self-describing for rehydration/archival lineage).
            // The value was read once at connect time and stashed on
            // `CameraEntry` — see its docstring for the connect-time-failure
            // semantics. When `None` we still persist the document with
            // `max_adu: None`, write the FITS as i32 (lossless fallback), and
            // skip the cache insert.
            let captured_max_adu: Option<u32> = cached_max_adu;

            let optics = derive_optics(camera_id, focal_length_mm, cached_optics);

            let cached_pixels = write_pixels(
                &image_path,
                image_array,
                (width, height),
                &document_id,
                captured_max_adu,
            )
            .await?;

            let doc = ExposureDocument {
                id: document_id.clone(),
                captured_at: captured_at.to_rfc3339(),
                file_path: image_path.clone(),
                width: doc_width,
                height: doc_height,
                camera_id: Some(camera_id.to_string()),
                duration: Some(duration),
                max_adu: captured_max_adu,
                cooler_setpoint_c,
                sensor_temperature_c,
                optics,
                target: exposure_target,
                frame_type: resolved_frame_type,
                sections: serde_json::Map::new(),
            };
            self.persist_capture_artifact(doc, cached_pixels, captured_max_adu)
                .await;

            Ok(())
        }
        .await;

        self.emit_exposure_outcome(
            &operation_id,
            started_at,
            &capture_result,
            &document_id,
            &image_path,
        );
        capture_result.map(|()| (image_path, document_id))
    }

    /// Emit the `exposure` started envelope with its predictive
    /// deadlines.
    fn emit_exposure_started(
        &self,
        operation_id: &str,
        started_at: chrono::DateTime<chrono::Utc>,
        camera_id: &str,
        duration: Duration,
        readout_time_estimate: Duration,
    ) {
        let (predicted_ms, max_ms) = exposure_deadlines(duration, readout_time_estimate);
        self.event_bus.emit_operation(
            EventEnvelope::started(
                "exposure",
                operation_id,
                started_at,
                serde_json::json!({
                    "camera_id": camera_id,
                    "duration": humantime::format_duration(duration).to_string(),
                }),
            )
            .with_deadlines(predicted_ms, max_ms),
        );
    }

    /// Mirror the `exposure` started envelope with exactly one of
    /// `exposure_complete` / `exposure_failed` under the shared
    /// `operation_id`.
    fn emit_exposure_outcome(
        &self,
        operation_id: &str,
        started_at: chrono::DateTime<chrono::Utc>,
        capture_result: &std::result::Result<(), String>,
        document_id: &str,
        image_path: &str,
    ) {
        match capture_result {
            Ok(()) => self.event_bus.emit_operation(EventEnvelope::complete(
                "exposure",
                operation_id,
                started_at,
                serde_json::json!({
                    "document_id": document_id,
                    "file_path": image_path,
                }),
            )),
            Err(e) => self.event_bus.emit_operation(EventEnvelope::failed(
                "exposure",
                operation_id,
                started_at,
                e,
            )),
        }
    }

    /// Persist the `wcs` section of a solve. `document_id` mode
    /// targets the resolved document directly; `image_path` mode reads
    /// the sibling `<base>.json` sidecar via
    /// `ImageCache::resolve_document_by_path` so the late-solve
    /// workflow's call (path-only, no `document_id` known to the
    /// caller) still updates the matching sidecar.
    async fn persist_wcs_section(
        &self,
        target_doc_id: Option<String>,
        fits_path: &str,
        outcome: &rp_plate_solver::SolveOutcome,
    ) {
        let payload = serde_json::json!({
            "ra_center": outcome.ra_center,
            "dec_center": outcome.dec_center,
            "pixel_scale_arcsec": outcome.pixel_scale_arcsec,
            "rotation_deg": outcome.rotation_deg,
            "solver": outcome.solver,
            "wcs_matrix": outcome.wcs_matrix,
        });
        let persist_doc_id = match target_doc_id {
            Some(id) => Some(id),
            None => self
                .image_cache
                .resolve_document_by_path(fits_path)
                .await
                .map(|d| d.id),
        };
        if let Some(doc_id) = persist_doc_id {
            if let Err(e) = self
                .image_cache
                .put_section(&doc_id, "wcs", payload.clone())
                .await
            {
                debug!(error = %e, document_id = %doc_id, "failed to persist wcs section");
            }
        } else {
            debug!(
                fits_path = %fits_path,
                "image_path did not resolve to a known document; skipping wcs persistence"
            );
        }
    }

    /// Cooling metadata (rp.md § Camera Cooling): the rung the
    /// controller currently holds for this camera, and a best-effort
    /// post-readout temperature read. Both are auxiliary — a failed
    /// read only drops the field, never the capture. Read after the
    /// exposure completes (rather than after the FITS write, as before
    /// Decision 11) because Decision 11's render step may need
    /// `sensor_temperature_c` to finalize `image_path` before the FITS
    /// write happens. `captured_at` is likewise anchored here — once,
    /// reused both for `{night_date}` and the document's `captured_at`
    /// field — rather than read twice a few hundred milliseconds
    /// apart.
    async fn capture_conditions(
        &self,
        camera_id: &str,
        cam: &Arc<dyn Camera>,
    ) -> (chrono::DateTime<chrono::Utc>, Option<i32>, Option<f64>) {
        let captured_at = chrono::Utc::now();
        let cooler_setpoint_c = self
            .cooling
            .as_ref()
            .and_then(|cooling| cooling.rung_for(camera_id));
        let sensor_temperature_c = cam.ccd_temperature().await.ok();
        (captured_at, cooler_setpoint_c, sensor_temperature_c)
    }

    /// Snapshot the connected camera handle, the train-derived focal
    /// length, and the five invariant physical-sensor properties
    /// cached at connect time. The `CameraEntry` is a borrow off
    /// `self.equipment`; the snapshot copies out the `Copy`/
    /// `Option<Copy>` values so the borrow does not have to outlive
    /// `do_capture`'s awaits — which is also what lets `do_capture`
    /// avoid the 5 Alpaca round-trips per exposure it used to pay for
    /// these properties (see `CameraEntry` docs). The readout estimate
    /// sizes the predictive exposure deadline (§2.4); omitted in
    /// config → the conservative built-in default. rp does not enforce
    /// it; it rides the `exposure_started` envelope for the Sentinel
    /// watchdog (the camera driver owns the exposure, and
    /// `CAPTURE_READOUT_GRACE` remains rp's own readout backstop).
    fn capture_snapshot(&self, camera_id: &str) -> std::result::Result<CaptureSnapshot, String> {
        let cam_entry = self
            .equipment
            .find_camera(camera_id)
            .ok_or_else(|| format!("camera not found: {camera_id}"))?;
        let cam = cam_entry
            .device
            .clone()
            .ok_or_else(|| format!("camera not connected: {camera_id}"))?;
        Ok(CaptureSnapshot {
            cam,
            focal_length_mm: self.trains.focal_length_for_camera(camera_id),
            readout_time_estimate: cam_entry
                .config
                .readout_time_estimate
                .unwrap_or(DEFAULT_READOUT_TIME_ESTIMATE),
            cached_max_adu: cam_entry.max_adu,
            cached_optics: (
                cam_entry.pixel_size_x_um,
                cam_entry.pixel_size_y_um,
                cam_entry.sensor_width_px,
                cam_entry.sensor_height_px,
            ),
        })
    }

    /// Poll until the frame is ready — but a not-ready camera is not
    /// necessarily still exposing. An Alpaca camera that *fails* an
    /// exposure transitions to `CameraState::Error` and leaves
    /// `ImageReady` false forever; polling `ImageReady` alone treats
    /// that as "still exposing" and loops indefinitely. That is the
    /// bug that ran CI's closed-loop centering BDD to GitHub's 6 h job
    /// cap: `sky-survey-camera`'s follow-mode mount read timed out
    /// under load, the exposure failed, and `do_capture` span here
    /// forever. Treat `Error` as terminal (surfacing the camera's
    /// stored reason via `image_array`), and cap the total wait with a
    /// deadline as a backstop for a camera wedged in `Exposing`.
    async fn wait_for_image_ready(
        cam: &Arc<dyn Camera>,
        duration: Duration,
        progress: Option<&dyn ProgressEmitter>,
    ) -> std::result::Result<(), String> {
        let started_at = Instant::now();
        let total_budget = duration.saturating_add(CAPTURE_READOUT_GRACE);
        // A budget too large for the clock degrades to an
        // already-expired deadline (immediate timeout), not a panic.
        let deadline = started_at.checked_add(total_budget).unwrap_or(started_at);
        let total_budget_secs = total_budget.as_secs_f64();
        // While `image_ready` returns `false` *before* the requested
        // exposure window elapses, the camera is shuttering. Switch the
        // emitted message to `"reading_out"` once we cross that mark —
        // most cameras hold `image_ready` false until the readout
        // download finishes too, which is when the keep-alive race is
        // most likely to bite (a long sky-survey download in CI). The
        // boundary is informational; the emit cadence is unchanged.
        let mut last_progress_at = started_at;
        let mut idle_streak: u32 = 0;
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            match cam.image_ready().await {
                Ok(true) => return Ok(()),
                Ok(false) => {
                    // A transient `camera_state` read error is non-fatal —
                    // `ImageReady` stays the primary signal and the deadline
                    // below still bounds the wait.
                    match cam.camera_state().await {
                        Ok(CameraState::Error) => {
                            let detail = cam.image_array().await.err().map_or_else(
                                || "camera reported error state".to_string(),
                                |e| e.to_string(),
                            );
                            return Err(format!("exposure failed: {detail}"));
                        }
                        // An aborted exposure (the safety enforcer's
                        // best-effort AbortExposure, an operator abort)
                        // returns the camera to Idle with no image —
                        // waiting out the readout backstop here would
                        // hold the shared motion-gate permit for the
                        // whole grace window, blocking the recovery
                        // slew that follows a safety interruption. Two
                        // consecutive Idle reads (plus a final
                        // ImageReady re-check) guard against a driver
                        // flapping through Idle as readout completes.
                        Ok(CameraState::Idle) => {
                            idle_streak = idle_streak.saturating_add(1);
                            if idle_streak >= 2 {
                                match cam.image_ready().await {
                                    Ok(true) => return Ok(()),
                                    Ok(false) => {
                                        return Err(
                                            "exposure aborted: camera is idle with no image"
                                                .to_string(),
                                        )
                                    }
                                    // A read error is a read error, not
                                    // an abort — same treatment as the
                                    // outer poll's Err arm.
                                    Err(e) => {
                                        return Err(format!("error checking image ready: {e}"))
                                    }
                                }
                            }
                        }
                        _ => idle_streak = 0,
                    }
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(format!(
                            "timeout waiting for image_ready after {total_budget:?}"
                        ));
                    }
                    if let Some(sink) = progress {
                        if now.duration_since(last_progress_at) >= PROGRESS_INTERVAL {
                            let elapsed = now.duration_since(started_at).as_secs_f64();
                            let phase = if now.duration_since(started_at) < duration {
                                "exposing"
                            } else {
                                "reading_out"
                            };
                            sink.emit(elapsed, Some(total_budget_secs), Some(phase.to_string()))
                                .await;
                            last_progress_at = now;
                        }
                    }
                }
                Err(e) => return Err(format!("error checking image ready: {e}")),
            }
        }
    }

    /// Decision 11 (rp.md § Capture Tool Details) naming: render the
    /// templated directory + file path for a typed frame. `Ok(None)`
    /// when no `session.file_naming_pattern` is configured — the frame
    /// keeps the flat `<doc_uuid_8>.fits` name.
    async fn render_templated_path(
        &self,
        cam: &Arc<dyn Camera>,
        target_slug: rp_targets::TargetSlug,
        ctx: TemplateRenderCtx<'_>,
    ) -> std::result::Result<Option<String>, String> {
        let Some(templates) = self.naming_templates.as_ref() else {
            return Ok(None);
        };
        let (filter_name, filter_position) = self
            .resolve_capture_filter(ctx.camera_id, ctx.frame_type)
            .await?;
        let bin = cam
            .bin()
            .await
            .map_err(|e| format!("capture: failed to read binning: {e}"))?;
        let binning = rp_targets::Binning {
            x: bin[0],
            y: bin[1],
        };
        let night_date = self
            .site
            .as_ref()
            .map(|site| site.night_date(ctx.captured_at));

        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            reason = "sensor temperatures are tens of degrees; `as` saturates at the i32 rails and the value only feeds a filename field"
        )]
        let sensor_temp_c = ctx.sensor_temperature_c.map(|t| t.round() as i32);
        let mut fields = naming_template::TemplateFields {
            target: Some(target_slug),
            filter: Some(filter_name.clone()),
            binning: Some(binning),
            exposure_duration: Some(ctx.duration),
            filter_position: Some(filter_position),
            sensor_temp_c,
            night_date,
            frame_type: Some(ctx.frame_type),
            ..Default::default()
        };

        let dir_relative = templates
            .directory
            .render(&fields)
            .map_err(|e| format!("capture: failed to render session.directory_pattern: {e}"))?;
        let scan_dir =
            std::path::Path::new(&self.session_config.data_directory).join(&dir_relative);
        let frame_number = next_frame_number(
            &templates.file,
            &scan_dir,
            &filter_name,
            binning,
            ctx.duration,
        )
        .await?;
        fields.frame_number = Some(frame_number);
        fields.uuid8 = Some(ctx.uuid8.to_string());

        let file_base = templates
            .file
            .render(&fields)
            .map_err(|e| format!("capture: failed to render session.file_naming_pattern: {e}"))?;

        Ok(Some(
            scan_dir
                .join(format!("{file_base}.fits"))
                .to_string_lossy()
                .into_owned(),
        ))
    }

    /// Resolves `do_capture`'s `target`/`frame_type` into the exposure
    /// document's `target` field plus the `TargetSlug` `render` needs
    /// (rp.md § Capture Tool Details, Decision 11). An explicit
    /// `target` always resolves against the store regardless of
    /// `frame_type` (an unknown slug or an absent store both error).
    /// Absent `target`: `Light` errors (a Light frame always needs a
    /// real target), `Dark`/`Flat`/`Bias` fall back to
    /// [`rp_vocabulary::FrameType::calibration_slug`].
    async fn resolve_capture_target(
        &self,
        target: Option<&str>,
        frame_type: FrameType,
    ) -> std::result::Result<(persistence::ExposureTarget, rp_targets::TargetSlug), String> {
        if let Some(target) = target {
            let slug = rp_targets::TargetSlug::new(target)
                .map_err(|e| format!("capture: invalid target slug '{target}': {e}"))?;
            let store = self
                .target_store
                .as_ref()
                .ok_or_else(|| "capture: target store not configured".to_string())?;
            let found = store
                .get_target(&slug)
                .await
                .map_err(|e| format!("capture: failed to look up target '{target}': {e}"))?
                .ok_or_else(|| format!("capture: unknown target '{target}'"))?;
            return Ok((persistence::ExposureTarget::from(&found), slug));
        }

        match frame_type.calibration_slug() {
            Some(reserved) => {
                // Infallible in practice — `calibration_slug`'s three
                // values are static lowercase-ASCII literals,
                // always valid `TargetSlug`s — but propagate rather than
                // `expect()` per this crate's no-panics-outside-tests rule.
                let slug = rp_targets::TargetSlug::new(reserved).map_err(|e| {
                    format!("internal: reserved calibration slug '{reserved}' is invalid: {e}")
                })?;
                let field = persistence::ExposureTarget {
                    slug: reserved.to_string(),
                    display_name: None,
                    ra_hours: None,
                    dec_degrees: None,
                };
                Ok((field, slug))
            }
            None => Err("capture: frame_type Light requires target".to_string()),
        }
    }

    /// Resolves `do_capture`'s `{filter}`/`{filter_position}` naming-
    /// template values: a live read from the resolved camera's train
    /// filter wheel for `Light`/`Flat` when one is present, else the
    /// fixed `"NA"`/`0` — always `"NA"`/`0` for `Dark`/`Bias`
    /// regardless of whether a wheel is present, since dark current
    /// isn't filter-dependent (rp.md § Capture Tool Details).
    async fn resolve_capture_filter(
        &self,
        camera_id: &str,
        frame_type: FrameType,
    ) -> std::result::Result<(String, u32), String> {
        let reads_live = matches!(frame_type, FrameType::Light | FrameType::Flat);
        if !reads_live {
            return Ok(("NA".to_string(), 0));
        }
        match self.live_filter(camera_id).await? {
            Some((name, position)) => Ok((name, position)),
            None => Ok(("NA".to_string(), 0)),
        }
    }

    /// Reads the live filter name + position from `camera_id`'s train,
    /// if it has a filter wheel. `Ok(None)` (not an error) when the
    /// camera isn't in a train, or the train has no filter wheel — the
    /// common case for mono/OSC rigs. Mirrors `get_filter`'s read
    /// logic (`built_in/filter_wheel.rs`).
    async fn live_filter(
        &self,
        camera_id: &str,
    ) -> std::result::Result<Option<(String, u32)>, String> {
        let Some(train) = self.trains.train_for_camera(camera_id) else {
            return Ok(None);
        };
        let Some(fw_id) = train
            .devices
            .iter()
            .find(|d| d.kind == TrainDeviceKind::FilterWheel)
            .map(|d| d.id.clone())
        else {
            return Ok(None);
        };
        let Some(fw_entry) = self.equipment.find_filter_wheel(&fw_id) else {
            return Ok(None);
        };
        let Some(fw) = fw_entry.device.clone() else {
            return Ok(None);
        };
        let position = fw
            .position()
            .await
            .map_err(|e| format!("failed to read filter wheel '{fw_id}' position: {e}"))?;
        let Some(position) = position else {
            return Err(format!("filter wheel '{fw_id}' is moving"));
        };
        let filter_name = fw_entry
            .config
            .filters
            .get(position)
            .cloned()
            .unwrap_or_else(|| format!("Filter {position}"));
        Ok(Some((
            filter_name,
            u32::try_from(position).unwrap_or(u32::MAX),
        )))
    }

    /// Size the predictive `move_focuser` deadline from the focuser's
    /// current position, the requested target, and the configured step rate
    /// (§2.3): `predicted = |target − current| / steps_per_sec`,
    /// `max = max(predicted × 2, MIN_FOCUSER_DEADLINE)`. Returns the poll
    /// deadline plus the `(predicted_ms, max_ms)` pair for the
    /// `move_focuser_started` envelope.
    ///
    /// `Err` if the focuser can't be resolved, the pre-move position read
    /// fails, or an absurdly small (but config-valid) step rate makes the
    /// deadline overflow `Duration` (`try_from_secs_f64`); the caller then
    /// falls back to [`FOCUSER_DEADLINE_FALLBACK`] and omits the envelope
    /// deadline fields.
    async fn compute_focuser_deadline(
        &self,
        focuser_id: &str,
        target: i32,
    ) -> std::result::Result<(Duration, u64, u64), String> {
        let foc_entry = self
            .equipment
            .find_focuser(focuser_id)
            .ok_or_else(|| format!("focuser not found: {focuser_id}"))?;
        let foc = foc_entry
            .device
            .as_ref()
            .ok_or_else(|| format!("focuser not connected: {focuser_id}"))?;
        let rate = foc_entry.config.steps_per_sec.value();
        let current = foc
            .position()
            .await
            .map_err(|e| format!("failed to read focuser position: {e}"))?;
        // The i64 difference of two i32s spans at most 2^32 − 1, which
        // both `u32` and (exactly) `f64` can carry.
        let distance_steps = i64::from(target)
            .saturating_sub(i64::from(current))
            .unsigned_abs();
        let distance = f64::from(u32::try_from(distance_steps).unwrap_or(u32::MAX));
        let predicted_secs = distance / rate;
        let max_secs =
            (predicted_secs * FOCUSER_DEADLINE_HEADROOM).max(MIN_FOCUSER_DEADLINE.as_secs_f64());
        let deadline = Duration::try_from_secs_f64(max_secs).map_err(|e| {
            format!(
                "predicted focuser deadline out of range \
                 (steps_per_sec {rate}, distance {distance} steps): {e}"
            )
        })?;
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "envelope milliseconds; `as` saturates at the u64 rails and `try_from_secs_f64` already rejected out-of-range budgets"
        )]
        let (predicted_ms, max_ms) = (
            (predicted_secs * 1000.0).round() as u64,
            (max_secs * 1000.0).round() as u64,
        );
        Ok((deadline, predicted_ms, max_ms))
    }

    /// Resolve a focuser, validate the requested `position` against the
    /// operator-supplied `min_position`/`max_position` bounds, issue the
    /// Alpaca move, poll `is_moving` until idle (bounded by a predicted
    /// deadline; see [`Self::compute_focuser_deadline`]), and return the
    /// focuser's reported `position` after settling.
    ///
    /// This is the shared body of the `move_focuser` MCP tool and the
    /// `auto_focus` compound tool's per-step focuser drive — both want
    /// the same bounds-check + blocking-poll semantics.
    ///
    /// When `progress` is `Some`, the `is_moving` poll loop emits
    /// `notifications/progress` every [`PROGRESS_INTERVAL`] so rmcp's
    /// 300 s session keep-alive sees session activity from a focuser
    /// run that approaches its own deadline. `None` (unit tests,
    /// clients without `progressToken`) makes the emission a no-op.
    pub(crate) async fn do_move_focuser_blocking(
        &self,
        focuser_id: &str,
        position: i32,
        progress: Option<&dyn ProgressEmitter>,
    ) -> std::result::Result<i32, String> {
        let operation_id = Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now();

        // Size the deadline from the move's actual workload. If the focuser
        // can't be resolved or the pre-move position read fails, fall back to
        // the historical 120 s ceiling and omit the deadline fields — a
        // prediction is an optimization, not a precondition for moving.
        let started_payload = serde_json::json!({ "focuser_id": focuser_id, "position": position });
        let (deadline, started_event) = match self
            .compute_focuser_deadline(focuser_id, position)
            .await
        {
            Ok((deadline, predicted_ms, max_ms)) => (
                deadline,
                EventEnvelope::started("move_focuser", &operation_id, started_at, started_payload)
                    .with_deadlines(predicted_ms, max_ms),
            ),
            Err(e) => {
                debug!(error = %e, "move_focuser deadline prediction unavailable; using fallback ceiling");
                (
                    FOCUSER_DEADLINE_FALLBACK,
                    EventEnvelope::started(
                        "move_focuser",
                        &operation_id,
                        started_at,
                        started_payload,
                    ),
                )
            }
        };
        self.event_bus.emit_operation(started_event);

        let result = self
            .do_move_focuser_blocking_inner(focuser_id, position, deadline, progress)
            .await;
        match &result {
            Ok(final_position) => self.event_bus.emit_operation(EventEnvelope::complete(
                "move_focuser",
                &operation_id,
                started_at,
                serde_json::json!({ "focuser_id": focuser_id, "position": final_position }),
            )),
            Err(e) => self.event_bus.emit_operation(EventEnvelope::failed(
                "move_focuser",
                &operation_id,
                started_at,
                e,
            )),
        }
        result
    }

    /// Inner body of [`do_move_focuser_blocking`] — resolve + bounds-check
    /// then move, poll until idle, and read back. Split out so the public
    /// method wraps it in the `move_focuser_started` /
    /// `move_focuser_complete` / `move_focuser_failed` triple. `deadline` is
    /// the predicted poll ceiling sized by the wrapper (see
    /// [`Self::compute_focuser_deadline`]).
    async fn do_move_focuser_blocking_inner(
        &self,
        focuser_id: &str,
        position: i32,
        deadline: Duration,
        progress: Option<&dyn ProgressEmitter>,
    ) -> std::result::Result<i32, String> {
        let foc_entry = self
            .equipment
            .find_focuser(focuser_id)
            .ok_or_else(|| format!("focuser not found: {focuser_id}"))?;
        let foc = foc_entry
            .device
            .clone()
            .ok_or_else(|| format!("focuser not connected: {focuser_id}"))?;

        if let Some(min) = foc_entry.config.min_position {
            if position < min {
                return Err(format!(
                    "position out of range: {position} < min_position {min}"
                ));
            }
        }
        if let Some(max) = foc_entry.config.max_position {
            if position > max {
                return Err(format!(
                    "position out of range: {position} > max_position {max}"
                ));
            }
        }

        debug!(focuser_id, position, "moving focuser");
        foc.move_(position)
            .await
            .map_err(|e| format!("failed to move focuser: {e}"))?;

        let total_budget = deadline;
        let total_budget_secs = total_budget.as_secs_f64();
        let started_at = Instant::now();
        // A budget too large for the clock degrades to an already-expired
        // deadline (immediate timeout), not a panic.
        let deadline = started_at.checked_add(total_budget).unwrap_or(started_at);
        let mut last_progress_at = started_at;
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            match foc.is_moving().await {
                Ok(false) => break,
                Ok(true) if Instant::now() < deadline => {
                    let now = Instant::now();
                    if let Some(sink) = progress {
                        if now.duration_since(last_progress_at) >= PROGRESS_INTERVAL {
                            let elapsed = now.duration_since(started_at).as_secs_f64();
                            sink.emit(
                                elapsed,
                                Some(total_budget_secs),
                                Some("focuser_moving".to_string()),
                            )
                            .await;
                            last_progress_at = now;
                        }
                    }
                }
                Ok(true) => return Err("timeout waiting for focuser to settle".to_string()),
                Err(e) => return Err(format!("error polling focuser is_moving: {e}")),
            }
        }

        foc.position()
            .await
            .map_err(|e| format!("failed to read focuser position: {e}"))
    }

    /// Resolve the singular mount, returning the entry + connected device
    /// or a string error matching the convention `resolve_device!` uses
    /// for `id`-keyed devices ("no mount configured" / "mount not
    /// connected"). Singular: no `id` parameter.
    pub(crate) fn resolve_mount(
        &self,
    ) -> std::result::Result<
        (
            &crate::equipment::MountEntry,
            Arc<dyn ascom_alpaca::api::Telescope>,
        ),
        String,
    > {
        let entry = self
            .equipment
            .find_mount()
            .ok_or_else(|| "no mount configured".to_string())?;
        let device = entry
            .device
            .clone()
            .ok_or_else(|| "mount not connected".to_string())?;
        Ok((entry, device))
    }

    /// Size the predictive slew deadline from the mount's current
    /// pointing, the requested target, the configured slew rate, and the
    /// settle time (§2.1). `ra` is in hours (the `slew` boundary unit),
    /// `dec` in degrees. Returns the poll deadline plus the
    /// `(predicted_ms, max_ms)` pair for the `slew_started` envelope.
    ///
    /// `Err` if the mount can't be resolved or a pre-slew pointing read
    /// fails; the caller then falls back to [`SLEW_DEADLINE_FALLBACK`] and
    /// omits the envelope deadline fields.
    async fn compute_slew_deadline(
        &self,
        ra: f64,
        dec: f64,
        settle_after: Duration,
    ) -> std::result::Result<(Duration, u64, u64), String> {
        let (entry, mount) = self.resolve_mount()?;
        let rate = entry.config.slew_rate_arcsec_per_sec.value();
        let current_ra = mount
            .right_ascension()
            .await
            .map_err(|e| format!("failed to read mount right_ascension: {e}"))?;
        let current_dec = mount
            .declination()
            .await
            .map_err(|e| format!("failed to read mount declination: {e}"))?;
        // `haversine_arcsec` takes degrees for both coordinates; RA is in
        // hours at this boundary, so scale both RA terms by 15 (matching
        // `center_on_target`).
        let distance_arcsec =
            imaging::haversine_arcsec(current_ra * 15.0, current_dec, ra * 15.0, dec);
        let predicted_secs = distance_arcsec / rate + settle_after.as_secs_f64();
        let max_secs = (predicted_secs * 3.0).max(MIN_SLEW_DEADLINE.as_secs_f64());
        // An absurdly small (but config-valid) rate makes distance / rate
        // huge — either +inf, or finite-but-larger than a `Duration` can
        // hold. `try_from_secs_f64` rejects non-finite, negative, AND
        // overflowing values, so we fall back to the 300 s ceiling rather
        // than panicking (which `Duration::from_secs_f64` would do for any
        // of those).
        let deadline = Duration::try_from_secs_f64(max_secs).map_err(|e| {
            format!(
                "predicted slew deadline out of range \
                 (slew_rate_arcsec_per_sec {rate}, distance {distance_arcsec} arcsec): {e}"
            )
        })?;
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "envelope milliseconds; `as` saturates at the u64 rails and `try_from_secs_f64` already rejected out-of-range budgets"
        )]
        let (predicted_ms, max_ms) = (
            (predicted_secs * 1000.0).round() as u64,
            (max_secs * 1000.0).round() as u64,
        );
        Ok((deadline, predicted_ms, max_ms))
    }

    /// Resolve the mount, issue an async slew, poll `slewing()` until
    /// idle (bounded by a predicted deadline; see
    /// [`Self::compute_slew_deadline`]), sleep `settle_after`, then read
    /// the post-slew RA/Dec and return them.
    ///
    /// Best-effort `abort_slew()` on deadline expiry before returning
    /// the timeout error — mount runaways have higher blast radius
    /// than focuser runaways (cables, hard stops, sun in a flat
    /// workflow).
    ///
    /// Mirrors `do_move_focuser_blocking`'s shape; same pass-through
    /// error mapping. Does NOT touch `Tracking` (per `mount.feature`
    /// + ASCOM contract — Tracking must already be on for
    ///   `slew_to_coordinates_async`).
    ///
    /// When `progress` is `Some`, the inner `poll_slewing_until_idle`
    /// and the `settle_after` sleep emit `notifications/progress`
    /// every [`PROGRESS_INTERVAL`] so rmcp's 300 s session keep-alive
    /// cannot fire during a legitimate long slew (whose deadline scales
    /// with distance and can exceed the 300 s keep-alive).
    pub(crate) async fn do_slew_blocking(
        &self,
        ra: f64,
        dec: f64,
        settle_after: Duration,
        progress: Option<&dyn ProgressEmitter>,
    ) -> std::result::Result<(f64, f64), String> {
        // Mount motion (rp.md § Mount Motion Gate): exclusive acquire
        // before the pre-slew pointing read, so the deadline predicted
        // from it never includes gate wait and stays honest.
        let _motion_permit = self.motion_gate.exclusive("slew").await;

        let operation_id = Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now();

        // Size the deadline from the slew's actual workload. If the mount
        // can't be resolved or the pre-slew pointing read fails, fall back
        // to the historical 300 s ceiling and omit the deadline fields —
        // a prediction is an optimization, not a precondition for slewing.
        let started_payload = serde_json::json!({ "ra": ra, "dec": dec });
        let (deadline, started_event) = match self
            .compute_slew_deadline(ra, dec, settle_after)
            .await
        {
            Ok((deadline, predicted_ms, max_ms)) => (
                deadline,
                EventEnvelope::started("slew", &operation_id, started_at, started_payload)
                    .with_deadlines(predicted_ms, max_ms),
            ),
            Err(e) => {
                debug!(error = %e, "slew deadline prediction unavailable; using fallback ceiling");
                (
                    SLEW_DEADLINE_FALLBACK,
                    EventEnvelope::started("slew", &operation_id, started_at, started_payload),
                )
            }
        };
        self.event_bus.emit_operation(started_event);

        let result = self
            .do_slew_blocking_inner(ra, dec, settle_after, deadline, progress)
            .await;
        match &result {
            Ok((actual_ra, actual_dec)) => self.event_bus.emit_operation(EventEnvelope::complete(
                "slew",
                &operation_id,
                started_at,
                serde_json::json!({
                    "ra": ra,
                    "dec": dec,
                    "actual_ra": actual_ra,
                    "actual_dec": actual_dec,
                }),
            )),
            Err(e) => self.event_bus.emit_operation(EventEnvelope::failed(
                "slew",
                &operation_id,
                started_at,
                e,
            )),
        }
        result
    }

    /// Inner body of [`do_slew_blocking`] — the slew + poll-until-idle +
    /// settle + post-slew read. Split out so the public method can wrap
    /// it in the `slew_started` / `slew_complete` / `slew_failed` event
    /// triple under one `operation_id`. Every call (including
    /// `center_on_target`'s per-iteration slews) emits its own triple;
    /// Sentinel filters inner-vs-outer in Phase 4. `deadline` is the
    /// predicted poll ceiling sized by the wrapper (see
    /// [`Self::compute_slew_deadline`]).
    async fn do_slew_blocking_inner(
        &self,
        ra: f64,
        dec: f64,
        settle_after: Duration,
        deadline: Duration,
        progress: Option<&dyn ProgressEmitter>,
    ) -> std::result::Result<(f64, f64), String> {
        let (_entry, mount) = self.resolve_mount()?;

        debug!(ra, dec, "slewing mount");
        mount
            .slew_to_coordinates_async(ra, dec)
            .await
            .map_err(|e| format!("failed to slew: {e}"))?;

        match poll_slewing_until_idle(mount.as_ref(), deadline, progress).await {
            Ok(()) => {}
            Err(PollIdleError::Timeout) => {
                // Best-effort abort; ignore the abort's own result and
                // surface the timeout error as the primary failure.
                let _ = mount.abort_slew().await;
                return Err("timeout waiting for mount to settle".to_string());
            }
            Err(PollIdleError::Read(e)) => {
                return Err(format!("error polling mount slewing: {e}"));
            }
        }

        if !settle_after.is_zero() {
            debug!(?settle_after, "waiting for mount settle");
            // For settles long enough to cross PROGRESS_INTERVAL, emit
            // a single tick so the session keep-alive can't fire
            // during the settle even when the upstream slew finished
            // quickly.
            if let Some(sink) = progress {
                if settle_after >= PROGRESS_INTERVAL {
                    sink.emit(
                        0.0,
                        Some(settle_after.as_secs_f64()),
                        Some("settling".to_string()),
                    )
                    .await;
                }
            }
            tokio::time::sleep(settle_after).await;
        }

        let actual_ra = mount
            .right_ascension()
            .await
            .map_err(|e| format!("failed to read mount right_ascension: {e}"))?;
        let actual_dec = mount
            .declination()
            .await
            .map_err(|e| format!("failed to read mount declination: {e}"))?;
        Ok((actual_ra, actual_dec))
    }

    /// Size the park deadline (§2.2). rp can't read the mount's park
    /// coordinates — the generic Alpaca `Telescope` trait exposes no
    /// park-position getter — so the deadline is the worst-case full-axis
    /// traverse ([`PARK_WORST_CASE_TRAVERSE_DEG`]) at the configured slew
    /// rate, not a distance-scaled prediction like slew:
    /// `predicted = 180° / slew_rate + settle`,
    /// `max = max(predicted × 2, MIN_PARK_DEADLINE)`. Returns the poll
    /// deadline plus the `(predicted_ms, max_ms)` pair for the
    /// `park_started` envelope.
    ///
    /// `Err` when no mount is configured, or when an absurdly small (but
    /// config-valid) slew rate makes the worst-case deadline overflow
    /// `Duration` (`try_from_secs_f64`). The caller then falls back to
    /// [`PARK_DEADLINE_FALLBACK`] and omits the envelope deadline fields
    /// (and park fails immediately anyway when no mount is configured).
    fn compute_park_deadline(&self) -> std::result::Result<(Duration, u64, u64), String> {
        let entry = self
            .equipment
            .find_mount()
            .ok_or_else(|| "no mount configured".to_string())?;
        let rate = entry.config.slew_rate_arcsec_per_sec.value();
        let settle = entry.config.settle_after_slew.unwrap_or(Duration::ZERO);
        let worst_case_arcsec = PARK_WORST_CASE_TRAVERSE_DEG * 3600.0;
        let predicted_secs = worst_case_arcsec / rate + settle.as_secs_f64();
        let max_secs =
            (predicted_secs * PARK_DEADLINE_HEADROOM).max(MIN_PARK_DEADLINE.as_secs_f64());
        let deadline = Duration::try_from_secs_f64(max_secs).map_err(|e| {
            format!("predicted park deadline out of range (slew_rate_arcsec_per_sec {rate}): {e}")
        })?;
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "envelope milliseconds; `as` saturates at the u64 rails and `try_from_secs_f64` already rejected out-of-range budgets"
        )]
        let (predicted_ms, max_ms) = (
            (predicted_secs * 1000.0).round() as u64,
            (max_secs * 1000.0).round() as u64,
        );
        Ok((deadline, predicted_ms, max_ms))
    }

    /// Resolve the mount, issue `park()`, then poll `at_park()` every
    /// 100 ms until it returns `true`, bounded by a predicted deadline
    /// (see [`Self::compute_park_deadline`]).
    ///
    /// `AtPark` is the ASCOM-canonical "park is complete" signal — set
    /// in exactly one code path (the slew-to-park completion handler).
    /// Polling `Slewing` would be over-conservative: ASCOM's
    /// `IsSlewing` is sticky on `MoveAxis`-driven rate state and any
    /// non-idle `SlewState`, so unrelated prior activity can keep it
    /// `true` even after `ChangePark(true)` has fired.
    ///
    /// Unlike `do_slew_blocking`, this does NOT auto-abort on timeout
    /// — a partially-completed park is closer to safe than an
    /// aborted one (the mount is actively trying to reach a known
    /// safe position; aborting leaves it in an unknown state
    /// mid-traversal). Callers that want to interrupt a stuck park
    /// can call the `abort_slew` MCP tool explicitly.
    ///
    /// Per ASCOM, a successful `park()` clears `Tracking`. We don't
    /// touch tracking ourselves; the contract is the driver's.
    ///
    /// When `progress` is `Some`, the `at_park` poll loop emits
    /// `notifications/progress` every [`PROGRESS_INTERVAL`] so rmcp's
    /// 300 s session keep-alive cannot fire during a legitimate
    /// long park (whose deadline can exceed the keep-alive).
    pub(crate) async fn do_park_blocking(
        &self,
        progress: Option<&dyn ProgressEmitter>,
    ) -> std::result::Result<(), String> {
        let operation_id = Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now();

        // Size the deadline from the worst-case traverse at the configured
        // slew rate. With no mount configured, fall back to the historical
        // 300 s ceiling and omit the deadline fields.
        let (deadline, started_event) = match self.compute_park_deadline() {
            Ok((deadline, predicted_ms, max_ms)) => (
                deadline,
                EventEnvelope::started("park", &operation_id, started_at, serde_json::json!({}))
                    .with_deadlines(predicted_ms, max_ms),
            ),
            Err(e) => {
                debug!(error = %e, "park deadline prediction unavailable; using fallback ceiling");
                (
                    PARK_DEADLINE_FALLBACK,
                    EventEnvelope::started(
                        "park",
                        &operation_id,
                        started_at,
                        serde_json::json!({}),
                    ),
                )
            }
        };
        self.event_bus.emit_operation(started_event);

        let result = self.do_park_blocking_inner(deadline, progress).await;
        match &result {
            Ok(()) => self.event_bus.emit_operation(EventEnvelope::complete(
                "park",
                &operation_id,
                started_at,
                serde_json::json!({}),
            )),
            Err(e) => self.event_bus.emit_operation(EventEnvelope::failed(
                "park",
                &operation_id,
                started_at,
                e,
            )),
        }
        result
    }

    /// Inner body of [`do_park_blocking`] — the `park()` call + the
    /// `at_park` poll loop. Split out so the public method wraps it in
    /// the `park_started` / `park_complete` / `park_failed` triple. The
    /// timeout path still returns `Err` (so `park_failed` fires) and
    /// still does NOT auto-abort — the watchdog ladder owns that decision
    /// in Phase 5. `deadline` is the predicted poll ceiling sized by the
    /// wrapper (see [`Self::compute_park_deadline`]).
    async fn do_park_blocking_inner(
        &self,
        deadline: Duration,
        progress: Option<&dyn ProgressEmitter>,
    ) -> std::result::Result<(), String> {
        let (_entry, mount) = self.resolve_mount()?;

        debug!("parking mount");
        mount
            .park()
            .await
            .map_err(|e| format!("failed to park: {e}"))?;

        let total_budget = deadline;
        let total_budget_secs = total_budget.as_secs_f64();
        let started_at = Instant::now();
        // A budget too large for the clock degrades to an already-expired
        // deadline (immediate timeout), not a panic.
        let deadline = started_at.checked_add(total_budget).unwrap_or(started_at);
        let mut last_progress_at = started_at;
        loop {
            match mount.at_park().await {
                Ok(true) => return Ok(()),
                Ok(false) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let now = Instant::now();
                    if let Some(sink) = progress {
                        if now.duration_since(last_progress_at) >= PROGRESS_INTERVAL {
                            let elapsed = now.duration_since(started_at).as_secs_f64();
                            sink.emit(
                                elapsed,
                                Some(total_budget_secs),
                                Some("parking".to_string()),
                            )
                            .await;
                            last_progress_at = now;
                        }
                    }
                }
                Ok(false) => return Err("timeout waiting for mount to park".to_string()),
                Err(e) => return Err(format!("error polling mount at_park: {e}")),
            }
        }
    }

    /// Resolve the mount and issue a sync to the given equatorial
    /// coordinates (RA hours, Dec degrees). No polling — `sync` is
    /// immediate per ASCOM. Mirrors the shape of `do_slew_blocking`
    /// minus the polling loop. Used by both the primitive
    /// `sync_mount` MCP tool and the `center_on_target` compound
    /// tool's per-iteration sync; one helper, one place to change
    /// the error-mapping convention.
    pub(crate) async fn do_sync_mount(
        &self,
        ra_hours: f64,
        dec_deg: f64,
    ) -> std::result::Result<(), String> {
        let operation_id = Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now();
        let result = self.do_sync_mount_inner(ra_hours, dec_deg).await;
        match &result {
            Ok(()) => self.event_bus.emit_operation(EventEnvelope::complete(
                "sync_mount",
                &operation_id,
                started_at,
                serde_json::json!({ "ra": ra_hours, "dec": dec_deg }),
            )),
            Err(e) => self.event_bus.emit_operation(EventEnvelope::failed(
                "sync_mount",
                &operation_id,
                started_at,
                e,
            )),
        }
        result
    }

    /// Inner body of [`do_sync_mount`]. Sync is instant per ASCOM, so the
    /// public method emits only `sync_mount_complete` /
    /// `sync_mount_failed` (no `_started` / timer).
    async fn do_sync_mount_inner(
        &self,
        ra_hours: f64,
        dec_deg: f64,
    ) -> std::result::Result<(), String> {
        let (_entry, mount) = self.resolve_mount()?;
        debug!(ra = ra_hours, dec = dec_deg, "syncing mount");
        mount
            .sync_to_coordinates(ra_hours, dec_deg)
            .await
            .map_err(|e| format!("failed to sync mount: {e}"))
    }

    /// Read the current mount pointing for `plate_solve`'s
    /// `use_mount_hints` convenience. Converts Alpaca's decimal
    /// hours `RightAscension` to the wrapper's degrees-on-the-wire
    /// contract (`× 15`); `Declination` passes through. Failure
    /// modes — no mount configured, mount not connected, Alpaca
    /// read error — surface as a single string the caller appends
    /// to its diagnostic.
    ///
    /// `center_on_target` issues this read every iteration; under heavy
    /// parallel-OmniSim CI load it was the read that stalled and hung
    /// the whole loop (issue #319). Both reads are idempotent, so they
    /// retry a transient failure via [`retry_idempotent_read`] rather
    /// than aborting the compound tool on a single hiccup; the
    /// per-request read timeout (see `equipment::alpaca`) bounds each
    /// attempt.
    pub(crate) async fn read_mount_hints_for_plate_solve(&self) -> Result<(f64, f64), String> {
        let (_entry, mount) = self.resolve_mount()?;
        let ra_hours = retry_idempotent_read("mount right_ascension", || {
            let mount = mount.clone();
            async move {
                mount
                    .right_ascension()
                    .await
                    .map_err(|e| format!("failed to read mount right_ascension: {e}"))
            }
        })
        .await?;
        let dec_deg = retry_idempotent_read("mount declination", || {
            let mount = mount.clone();
            async move {
                mount
                    .declination()
                    .await
                    .map_err(|e| format!("failed to read mount declination: {e}"))
            }
        })
        .await?;
        Ok((ra_hours * 15.0, dec_deg))
    }

    /// Shared body of the standalone `plate_solve` MCP tool *and* the
    /// `center_on_target` compound tool's per-iteration solve. Both
    /// callers want the same configured-check, document resolution,
    /// hint sourcing, request build, error mapping, and `wcs`
    /// persistence — extracting them here keeps any future change to
    /// defaults / validation / persistence in exactly one place.
    ///
    /// Caller responsibilities:
    /// - Standalone `plate_solve` validates "neither `document_id`
    ///   nor `image_path` supplied" itself (so the error message
    ///   shape matches what its BDD pins).
    /// - `center_on_target` always supplies `document_id` and
    ///   hardcodes `pointing_hint: None, use_mount_hints: true`.
    pub(crate) async fn do_plate_solve(
        &self,
        input: DoPlateSolveInput<'_>,
    ) -> Result<DoPlateSolveOutput, String> {
        let operation_id = Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now();
        self.event_bus.emit_operation(EventEnvelope::started(
            "plate_solve",
            &operation_id,
            started_at,
            serde_json::json!({
                "document_id": input.document_id,
                "image_path": input.image_path,
                "use_mount_hints": input.use_mount_hints,
            }),
        ));
        let result = self.do_plate_solve_inner(input).await;
        match &result {
            Ok(out) => self.event_bus.emit_operation(EventEnvelope::complete(
                "plate_solve",
                &operation_id,
                started_at,
                serde_json::json!({
                    "ra_center": out.ra_center,
                    "dec_center": out.dec_center,
                    "pixel_scale_arcsec": out.pixel_scale_arcsec,
                    "rotation_deg": out.rotation_deg,
                    "solver": out.solver,
                }),
            )),
            Err(e) => self.event_bus.emit_operation(EventEnvelope::failed(
                "plate_solve",
                &operation_id,
                started_at,
                e,
            )),
        }
        result
    }

    /// Inner body of [`do_plate_solve`]. Split out so the public method
    /// wraps it in the `plate_solve_started` / `plate_solve_complete` /
    /// `plate_solve_failed` triple under one `operation_id`.
    async fn do_plate_solve_inner(
        &self,
        input: DoPlateSolveInput<'_>,
    ) -> Result<DoPlateSolveOutput, String> {
        // Hint validation: pointing_hint and use_mount_hints=true are
        // mutually exclusive. center_on_target hardcodes
        // pointing_hint=None so it never trips this; the standalone
        // tool may.
        if input.pointing_hint.is_some() && input.use_mount_hints {
            return Err(
                "plate_solve: provide explicit pointing_hint or use_mount_hints, not both"
                    .to_string(),
            );
        }

        let client = self
            .plate_solver
            .clone()
            .ok_or_else(|| "plate_solve: plate solver not configured".to_string())?;

        // Resolve fits_path: document_id wins when both supplied.
        let (fits_path, target_doc_id) = if let Some(doc_id) = input.document_id {
            match self.image_cache.resolve_document(doc_id).await {
                Some(doc) => (doc.file_path, Some(doc_id.to_string())),
                None => return Err(format!("plate_solve: document not found: {doc_id}")),
            }
        } else {
            let path = input.image_path.ok_or_else(|| {
                "plate_solve: missing required argument: provide either document_id or image_path"
                    .to_string()
            })?;
            (path.to_string(), None)
        };

        // Resolve hints. The wrapper takes flat ra_hint/dec_hint in
        // decimal degrees; the mount-hint helper does the Alpaca-
        // hours → degrees ×15 conversion.
        let (ra_hint, dec_hint) = if let Some((ra_deg, dec_deg)) = input.pointing_hint {
            (Some(ra_deg), Some(dec_deg))
        } else if input.use_mount_hints {
            match self.read_mount_hints_for_plate_solve().await {
                Ok((ra_deg, dec_deg)) => (Some(ra_deg), Some(dec_deg)),
                Err(e) => return Err(format!("plate_solve: use_mount_hints requested but {e}")),
            }
        } else {
            (None, None)
        };

        // search_radius_deg: per-call value > config default > absent.
        let search_radius_deg = input
            .search_radius_deg
            .or(self.plate_solver_default_search_radius_deg);

        let request = rp_plate_solver::SolveRequest {
            fits_path: fits_path.clone(),
            ra_hint,
            dec_hint,
            fov_hint_deg: input.fov_hint_deg,
            search_radius_deg,
            timeout: input.timeout,
        };

        let outcome = match client.solve(request).await {
            Ok(o) => o,
            Err(rp_plate_solver::SolveError::ServiceUnreachable(reason)) => {
                return Err(format!("plate_solve: service unreachable: {reason}"));
            }
            Err(rp_plate_solver::SolveError::Wrapper {
                code,
                message,
                details,
            }) => {
                if details.is_null() {
                    return Err(format!("plate_solve: {code}: {message}"));
                }
                return Err(format!(
                    "plate_solve: {code}: {message} (details: {details})"
                ));
            }
            Err(rp_plate_solver::SolveError::Internal(reason)) => {
                return Err(format!("plate_solve: internal: {reason}"));
            }
        };

        self.persist_wcs_section(target_doc_id, &fits_path, &outcome)
            .await;

        Ok(DoPlateSolveOutput {
            ra_center: outcome.ra_center,
            dec_center: outcome.dec_center,
            pixel_scale_arcsec: outcome.pixel_scale_arcsec,
            rotation_deg: outcome.rotation_deg,
            solver: outcome.solver,
            wcs_matrix: outcome.wcs_matrix,
        })
    }
}

/// Input bundle for [`McpHandler::do_plate_solve`]. Borrows the two
/// path-shaped inputs by `&str` (call sites already own the strings)
/// and takes the rest by value (all small `Copy` / `Option` types).
pub(crate) struct DoPlateSolveInput<'a> {
    pub document_id: Option<&'a str>,
    pub image_path: Option<&'a str>,
    /// Decimal degrees `(ra, dec)`. Mutually exclusive with
    /// `use_mount_hints == true`.
    pub pointing_hint: Option<(f64, f64)>,
    pub use_mount_hints: bool,
    pub fov_hint_deg: Option<f64>,
    pub search_radius_deg: Option<f64>,
    pub timeout: Option<Duration>,
}

/// Output of [`McpHandler::do_plate_solve`]: the wrapper's success
/// fields verbatim. Callers wrap this in a `tool_success!` payload
/// or in a [`crate::imaging::tools::center_on_target::SolveOutcome`]
/// as needed.
pub(crate) struct DoPlateSolveOutput {
    pub ra_center: f64,
    pub dec_center: f64,
    pub pixel_scale_arcsec: f64,
    pub rotation_deg: f64,
    pub solver: String,
    /// Full CRPIX + CD-matrix mapping from the wrapper; `None` when
    /// its `.wcs` sidecar lacked a complete six-key set.
    pub wcs_matrix: Option<rp_plate_solver::WcsMatrix>,
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

pub(crate) fn stats_outcome<T: imaging::Pixel>(
    view: &ndarray::ArrayView2<T>,
) -> crate::error::Result<imaging::ImageStats> {
    // `compute_stats` is typed on `&mut [i32]` and uses
    // `select_nth_unstable` in place. Materialize a flat i32 buffer
    // once here and hand it to the kernel mutably so the cached-pixel
    // path doesn't pay the second n × 4 bytes that an immutable slice
    // signature would force (caller copy + kernel-internal clone).
    // Negative pixels are clamped to 0 inside `compute_stats`, so the
    // `to_u32() as i32` round-trip is safe for realistic camera
    // ranges (u16 cameras + i32 scientific HDR ≤ i32::MAX).
    let mut pixels: Vec<i32> = view.iter().map(|p| p.to_u32().cast_signed()).collect();
    imaging::compute_stats(&mut pixels)
        .ok_or_else(|| crate::error::RpError::Imaging("image has no pixels".into()))
}

pub(crate) fn clip_outcome<T: imaging::Pixel>(
    view: &ndarray::ArrayView2<T>,
    params: &ResolvedClipParams,
) -> crate::error::Result<BackgroundOutcome> {
    let (rows, cols) = view.dim();
    let total_pixels = u64::try_from(rows)
        .unwrap_or(u64::MAX)
        .saturating_mul(u64::try_from(cols).unwrap_or(u64::MAX));
    let stats =
        imaging::sigma_clipped_stats(view, params.k, params.max_iters).ok_or_else(|| {
            crate::error::RpError::Imaging("background estimation failed".to_string())
        })?;
    Ok(BackgroundOutcome {
        stats,
        total_pixels,
    })
}

pub(crate) fn detect_outcome<T: imaging::Pixel>(
    view: &ndarray::ArrayView2<T>,
    params: &ResolvedDetectParams,
    max_adu: Option<u32>,
) -> crate::error::Result<DetectStarsOutcome> {
    let background = imaging::estimate_background(view).ok_or_else(|| {
        crate::error::RpError::Imaging("background estimation failed".to_string())
    })?;

    let detection = DetectionParams {
        threshold_sigma: params.threshold_sigma,
        smoothing_sigma: 1.0,
        min_area: params.min_area,
        max_area: params.max_area,
        max_adu,
    };
    let stars = imaging::detect_stars(view, &background, &detection);
    Ok(DetectStarsOutcome { stars, background })
}

pub(crate) fn star_to_json(s: &Star) -> serde_json::Value {
    serde_json::json!({
        "x": s.centroid_x,
        "y": s.centroid_y,
        "flux": s.total_flux,
        "peak": s.peak,
        "saturated_pixel_count": s.saturated_pixel_count,
    })
}

/// Outcome variants for [`poll_slewing_until_idle`].
#[derive(Debug)]
pub(crate) enum PollIdleError {
    /// Deadline expired with `slewing()` still returning `true`.
    Timeout,
    /// `slewing()` itself returned an Alpaca error.
    Read(ascom_alpaca::ASCOMError),
}

/// Consecutive `slewing()` read errors [`poll_slewing_until_idle`]
/// tolerates before giving up. A transient read failure — the now
/// timeout-bounded stall a loaded `OmniSim` or a flaky link produces
/// (issue #319) — is treated like "not idle yet" and retried on the
/// next tick; only a *persistent* failure aborts the slew. The 300 s
/// deadline still caps the total wait. Mirrors the connect path's
/// tolerance for a transient device stall.
const SLEWING_READ_ERROR_TOLERANCE: u32 = 5;

/// Poll `mount.slewing()` every 100 ms until it returns `false`,
/// bounded by `deadline`. `do_slew_blocking` sizes the deadline from the
/// slew distance (see `compute_slew_deadline`); a flaky pre-slew read
/// falls back to `SLEW_DEADLINE_FALLBACK`. (The sibling
/// `do_park_blocking` polls `at_park()` directly rather than
/// `slewing()` because `IsSlewing` is sticky on `MoveAxis` rate
/// state and `AtPark` is the ASCOM-canonical "park is complete"
/// signal — see the comment on `do_park_blocking`.) On
/// [`PollIdleError::Timeout`] the caller decides whether to
/// best-effort `abort_slew()` (slew does) or just surface the
/// timeout.
///
/// A transient `slewing()` read error is tolerated (kept polling) up to
/// [`SLEWING_READ_ERROR_TOLERANCE`] consecutive failures so a brief
/// device hiccup mid-slew doesn't abort the whole `center_on_target`
/// loop; a successful read resets the counter.
///
/// When `progress` is `Some`, the loop emits
/// `notifications/progress` every [`PROGRESS_INTERVAL`] so rmcp's
/// 300 s session keep-alive cannot fire during a legitimate slew
/// (a long slew's `deadline` can exceed the 300 s keep-alive — without
/// progress emission the two timers race).
pub(crate) async fn poll_slewing_until_idle(
    mount: &(dyn ascom_alpaca::api::Telescope + Send + Sync),
    deadline: Duration,
    progress: Option<&dyn ProgressEmitter>,
) -> std::result::Result<(), PollIdleError> {
    let total_budget = deadline;
    let total_budget_secs = total_budget.as_secs_f64();
    let started_at = Instant::now();
    // A budget too large for the clock degrades to an already-expired
    // deadline (immediate timeout), not a panic.
    let deadline = started_at.checked_add(total_budget).unwrap_or(started_at);
    let mut last_progress_at = started_at;
    let mut consecutive_read_errors: u32 = 0;
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        match mount.slewing().await {
            Ok(false) => return Ok(()),
            Ok(true) if Instant::now() < deadline => {
                consecutive_read_errors = 0;
                let now = Instant::now();
                if let Some(sink) = progress {
                    if now.duration_since(last_progress_at) >= PROGRESS_INTERVAL {
                        let elapsed = now.duration_since(started_at).as_secs_f64();
                        sink.emit(
                            elapsed,
                            Some(total_budget_secs),
                            Some("slewing".to_string()),
                        )
                        .await;
                        last_progress_at = now;
                    }
                }
            }
            Ok(true) => return Err(PollIdleError::Timeout),
            Err(e) => {
                consecutive_read_errors = consecutive_read_errors.saturating_add(1);
                if consecutive_read_errors >= SLEWING_READ_ERROR_TOLERANCE
                    || Instant::now() >= deadline
                {
                    return Err(PollIdleError::Read(e));
                }
                debug!(
                    consecutive_read_errors,
                    max = SLEWING_READ_ERROR_TOLERANCE,
                    error = %e,
                    "transient mount slewing() read error, continuing to poll"
                );
            }
        }
    }
}

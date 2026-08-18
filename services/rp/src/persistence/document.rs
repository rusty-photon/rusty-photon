//! Exposure documents: the JSON record describing a captured image plus all
//! tool-produced sections written against it.
//!
//! `rp` owns the core document fields (id, `captured_at`, `file_path`, camera/
//! exposure metadata). Image-analysis tools and plugins contribute additional
//! data via named sections — see `docs/services/rp.md` (Exposure Document and
//! Plugin Sections).
//!
//! Persistence is a sidecar JSON file written next to each FITS file
//! (`<image>.json`). Writes are atomic: data is staged into a `.tmp` file and
//! `rename`d into place, so a crash mid-write cannot leave a torn document.
//!
//! As of Phase 7 (`docs/plans/archive/image-evaluation-tools.md`), the document lives
//! inline on each `imaging::CachedImage` cache entry. Lookups go through
//! `ImageCache::get_document` and section updates through
//! `ImageCache::put_section`; the sidecar JSON pair on disk plus the disk-
//! fallback resolution path together provide the "live as long as the file
//! is on disk" contract.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{Result, RpError};

/// A captured exposure plus any sections tools have written against it.
///
/// `sections` is an open map keyed by tool/plugin name (`image_analysis`,
/// `flat_calibration`, `plate_solve`, etc.). Each section's shape is owned by
/// its writer; `rp` does not validate them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExposureDocument {
    pub id: String,
    /// RFC3339 timestamp of capture completion.
    pub captured_at: String,
    /// Absolute path to the FITS file on disk.
    pub file_path: String,
    pub width: u32,
    pub height: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "humantime_serde"
    )]
    pub duration: Option<Duration>,
    /// Camera's `MaxADU` at the time of capture. The sidecar carries it
    /// forward so a disk-fallback rehydration of the image cache can
    /// pick the correct `CachedPixels` variant without needing the
    /// originating camera to still be connected — see Phase 7
    /// (`docs/plans/archive/image-evaluation-tools.md`) and the Image and
    /// Document Cache section of `docs/services/rp.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_adu: Option<u32>,
    /// The dark-library rung rp was regulating the capturing camera at
    /// (rp.md § Camera Cooling). Omitted when rp was not cooling it —
    /// empty ladder, cooling skipped or unreachable, or a warm-up in
    /// progress. Ties the frame to its dark library.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooler_setpoint_c: Option<i32>,
    /// Best-effort `CCDTemperature` read at capture time. Omitted when
    /// the read fails or the camera does not implement it. Together
    /// with `cooler_setpoint_c` this makes a night where cooling
    /// misbehaved identifiable frame by frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensor_temperature_c: Option<f64>,
    /// Optical-train geometry resolved at capture time. Carries both the raw
    /// Alpaca camera readings (`pixel_size_*_um`, `sensor_*_px`) and the
    /// derived pixel scale and FOV that consumers like `plate_solve` and
    /// annotation tools want without re-deriving from the FITS header.
    /// Omitted when any input was missing — see `docs/services/rp.md`
    /// §"Core Fields".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optics: Option<Optics>,
    /// The sky target this frame belongs to (Decision 11), resolved by
    /// `capture` when its `frame_type` parameter is supplied — a
    /// target-store lookup for `Light` frames, or a reserved slug for
    /// `Dark`/`Flat`/`Bias` absent an explicit `target`. Omitted
    /// (absent, not `null`) when `frame_type` was omitted — today's
    /// flat `<doc_uuid_8>.fits` capture path. See `docs/services/rp.md`
    /// §"Capture Tool Details".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ExposureTarget>,
    /// This capture's `frame_type` parameter, verbatim. Omitted under
    /// the same condition as `target`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_type: Option<rp_vocabulary::FrameType>,
    #[serde(default)]
    pub sections: Map<String, Value>,
}

/// The exposure document's `target` field (Decision 11). `display_name`/
/// `ra_hours`/`dec_degrees` are populated only when `slug` resolved
/// against a real target-store row — `None` for a `Dark`/`Flat`/`Bias`
/// capture's reserved slug, which names no store entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExposureTarget {
    pub slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ra_hours: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dec_degrees: Option<f64>,
}

impl From<&rp_targets::Target> for ExposureTarget {
    fn from(t: &rp_targets::Target) -> Self {
        Self {
            slug: t.slug.as_str().to_string(),
            display_name: Some(t.display_name.clone()),
            ra_hours: Some(t.coord.ra_hours()),
            dec_degrees: Some(t.coord.dec_degrees()),
        }
    }
}

/// Optical-train geometry persisted on the exposure document at capture
/// time. See [`ExposureDocument::optics`] and `docs/services/rp.md`
/// §"Core Fields" for the derivation, the failure modes, and the
/// `plate_solve` consumer contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Optics {
    /// Operator-supplied effective focal length of the optical train,
    /// in millimetres (verbatim from `equipment.cameras[].focal_length_mm`).
    pub focal_length_mm: f64,
    /// Raw Alpaca `PixelSizeX` reading at capture time, in microns.
    pub pixel_size_x_um: f64,
    /// Raw Alpaca `PixelSizeY` reading at capture time, in microns.
    pub pixel_size_y_um: f64,
    /// Raw Alpaca `CameraXSize` reading at capture time, in pixels.
    pub sensor_width_px: u32,
    /// Raw Alpaca `CameraYSize` reading at capture time, in pixels.
    pub sensor_height_px: u32,
    /// Derived: `206.265 × pixel_size_x_um / focal_length_mm`.
    pub pixel_scale_x_arcsec_per_pixel: f64,
    /// Derived: `206.265 × pixel_size_y_um / focal_length_mm`.
    pub pixel_scale_y_arcsec_per_pixel: f64,
    /// Derived: `pixel_scale_x_arcsec_per_pixel × sensor_width_px / 3600`.
    pub fov_width_deg: f64,
    /// Derived: `pixel_scale_y_arcsec_per_pixel × sensor_height_px / 3600`.
    /// Matches ASTAP's `-fov` semantics ("image height in degrees"); used
    /// as the `plate_solve` `fov_hint_deg` default in `document_id` mode.
    pub fov_height_deg: f64,
}

/// The raw inputs [`Optics::from_camera_geometry`] derives from: the
/// operator-supplied focal length plus the camera-reported pixel size
/// and sensor dimensions.
#[derive(Clone, Copy, Debug)]
pub struct CameraGeometry {
    pub focal_length_mm: f64,
    pub pixel_size_x_um: f64,
    pub pixel_size_y_um: f64,
    pub sensor_width_px: u32,
    pub sensor_height_px: u32,
}

impl Optics {
    /// Compute pixel scale + FOV from the camera geometry. Returns
    /// `None` when any input — or any derived value — is non-finite or
    /// non-positive. Callers log at `debug!` and persist the document
    /// without an `optics` block.
    ///
    /// Derived values are validated because `serde_json` rejects
    /// non-finite floats: a sub-normal `focal_length_mm` could push the
    /// pixel scale to `inf`, which would then fail the *entire* sidecar
    /// write — breaking capture's persistence contract for an auxiliary
    /// metadata block. Defense in depth keeps the failure scoped to the
    /// `optics` field.
    #[must_use]
    pub fn from_camera_geometry(geometry: CameraGeometry) -> Option<Self> {
        let positive = |v: f64| v.is_finite() && v > 0.0;
        if !positive(geometry.focal_length_mm)
            || !positive(geometry.pixel_size_x_um)
            || !positive(geometry.pixel_size_y_um)
            || geometry.sensor_width_px == 0
            || geometry.sensor_height_px == 0
        {
            return None;
        }
        let pixel_scale_x = 206.265 * geometry.pixel_size_x_um / geometry.focal_length_mm;
        let pixel_scale_y = 206.265 * geometry.pixel_size_y_um / geometry.focal_length_mm;
        let fov_width_deg = pixel_scale_x * f64::from(geometry.sensor_width_px) / 3600.0;
        let fov_height_deg = pixel_scale_y * f64::from(geometry.sensor_height_px) / 3600.0;
        if !positive(pixel_scale_x)
            || !positive(pixel_scale_y)
            || !positive(fov_width_deg)
            || !positive(fov_height_deg)
        {
            return None;
        }
        Some(Self {
            focal_length_mm: geometry.focal_length_mm,
            pixel_size_x_um: geometry.pixel_size_x_um,
            pixel_size_y_um: geometry.pixel_size_y_um,
            sensor_width_px: geometry.sensor_width_px,
            sensor_height_px: geometry.sensor_height_px,
            pixel_scale_x_arcsec_per_pixel: pixel_scale_x,
            pixel_scale_y_arcsec_per_pixel: pixel_scale_y,
            fov_width_deg,
            fov_height_deg,
        })
    }
}

/// Sidecar JSON path for a given FITS file path (`/foo/<uuid8>.fits` →
/// `/foo/<uuid8>.json`).
#[must_use]
pub fn sidecar_path(file_path: &str) -> PathBuf {
    let p = PathBuf::from(file_path);
    p.with_extension("json")
}

/// Read a sidecar JSON file from disk and deserialize it into an
/// `ExposureDocument`. Synchronous because sidecars are small (single-digit
/// KB even with measurement sections); the disk-fallback resolver runs the
/// whole scan on the blocking pool already.
pub fn read_sidecar_sync(path: &Path) -> Result<ExposureDocument> {
    let body = std::fs::read(path).map_err(|e| {
        RpError::Imaging(format!(
            "failed to read sidecar '{}': {}",
            path.display(),
            e
        ))
    })?;
    serde_json::from_slice(&body).map_err(|e| {
        RpError::Imaging(format!(
            "failed to parse sidecar '{}': {}",
            path.display(),
            e
        ))
    })
}

/// Atomically write `doc` to its sidecar JSON path
/// (`<doc.file_path>.with_extension("json")`).
///
/// Stages into a sibling temp file, fsyncs, renames into place, fsyncs the
/// parent directory (unix-only). Mirrors the FITS write treatment in
/// `imaging::fits::write_fits`.
pub async fn write_sidecar(doc: &ExposureDocument) -> Result<()> {
    let path = sidecar_path(&doc.file_path);
    write_sidecar_at(&path, doc).await
}

/// As `write_sidecar`, but writes to an explicit path. Used by tests that
/// want to verify sidecar I/O without round-tripping through the rest of
/// the pipeline.
pub async fn write_sidecar_at(path: &Path, doc: &ExposureDocument) -> Result<()> {
    let body = serde_json::to_vec_pretty(doc)?;
    let path = path.to_path_buf();
    // Run the whole stage-and-commit sequence on the blocking pool. Matches
    // the `imaging::fits::write_fits` pattern: one task spawn per write rather
    // than one per `tokio::fs::*` call, and lets us use sync-only crates like
    // `tempfile` for the staging file.
    tokio::task::spawn_blocking(move || write_sidecar_sync(&path, &body))
        .await
        .map_err(|e| RpError::Imaging(format!("sidecar write task join error: {e}")))?
}

fn write_sidecar_sync(final_path: &Path, body: &[u8]) -> Result<()> {
    let parent = final_path.parent().ok_or_else(|| {
        RpError::Imaging(format!(
            "sidecar path has no parent: {}",
            final_path.display()
        ))
    })?;
    std::fs::create_dir_all(parent)?;

    // `NamedTempFile::new_in(parent)` gives us an OS-generated unique name
    // (so two concurrent writers can't collide on the staging path) and a
    // `Drop` guard that removes the staging file on panic or early return.
    // `persist` disarms the guard on success.
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(body)?;
    // fsync the file data so a crash after rename cannot surface a renamed-
    // but-zero-length sidecar.
    tmp.as_file().sync_all()?;
    tmp.persist(final_path).map_err(|e| RpError::Io(e.error))?;
    // fsync the parent directory so the rename itself is durable. Windows
    // can't open a directory as a regular file handle, so this is unix-only.
    #[cfg(unix)]
    {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn doc_with_path(id: &str, file_path: &str) -> ExposureDocument {
        ExposureDocument {
            id: id.to_string(),
            captured_at: "2026-04-28T12:00:00Z".to_string(),
            file_path: file_path.to_string(),
            width: 16,
            height: 16,
            camera_id: Some("cam".to_string()),
            duration: Some(Duration::from_secs(1)),
            max_adu: Some(65535),
            cooler_setpoint_c: None,
            sensor_temperature_c: None,
            optics: None,
            target: None,
            frame_type: None,
            sections: Map::new(),
        }
    }

    #[tokio::test]
    async fn write_sidecar_round_trips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let fits_path = dir.path().join("img.fits").to_string_lossy().into_owned();
        let sidecar = dir.path().join("img.json");

        let doc = doc_with_path("doc-1", &fits_path);
        write_sidecar(&doc).await.unwrap();

        let body = tokio::fs::read_to_string(&sidecar).await.unwrap();
        let parsed: ExposureDocument = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.id, "doc-1");
        assert_eq!(parsed.file_path, fits_path);
        assert_eq!(parsed.max_adu, Some(65535));
    }

    #[tokio::test]
    async fn write_sidecar_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let fits_path = dir.path().join("img.fits").to_string_lossy().into_owned();
        let sidecar = dir.path().join("img.json");

        let mut doc = doc_with_path("doc-1", &fits_path);
        doc.duration = Some(Duration::from_secs(1));
        write_sidecar(&doc).await.unwrap();

        doc.duration = Some(Duration::from_secs(2));
        write_sidecar(&doc).await.unwrap();

        let body = tokio::fs::read_to_string(&sidecar).await.unwrap();
        let parsed: ExposureDocument = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.duration, Some(Duration::from_secs(2)));
    }

    async fn entry_names(dir: &std::path::Path) -> Vec<String> {
        let mut entries = tokio::fs::read_dir(dir).await.unwrap();
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        names
    }

    #[tokio::test]
    async fn successful_write_leaves_no_staging_files() {
        let dir = tempfile::tempdir().unwrap();
        let fits_path = dir.path().join("img.fits").to_string_lossy().into_owned();

        let doc = doc_with_path("doc-1", &fits_path);
        write_sidecar(&doc).await.unwrap();

        let names = entry_names(dir.path()).await;
        assert_eq!(
            names,
            vec!["img.json"],
            "directory should contain only the sidecar after a successful write"
        );
    }

    #[tokio::test]
    async fn failed_write_cleans_up_staging_file() {
        // Force the rename to fail by replacing the destination with a
        // directory — `rename(file, dir)` is rejected on Linux and Windows.
        let dir = tempfile::tempdir().unwrap();
        let fits_path = dir.path().join("img.fits").to_string_lossy().into_owned();
        let sidecar = dir.path().join("img.json");

        tokio::fs::create_dir(&sidecar).await.unwrap();

        let doc = doc_with_path("doc-1", &fits_path);
        let err = write_sidecar(&doc).await.unwrap_err();
        assert!(
            !err.to_string().is_empty(),
            "expected non-empty write failure error"
        );

        let names = entry_names(dir.path()).await;
        assert_eq!(
            names,
            vec!["img.json"],
            "failed write must not leave a staging file behind (only the directory we put in the way remains)"
        );
    }

    #[test]
    fn sidecar_path_swaps_extension() {
        assert_eq!(
            sidecar_path("/data/lights/550e8400.fits"),
            std::path::PathBuf::from("/data/lights/550e8400.json")
        );
    }

    #[test]
    fn read_sidecar_sync_errors_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does_not_exist.json");
        let err = read_sidecar_sync(&missing).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("failed to read sidecar"),
            "expected read-failure prefix, got: {msg}"
        );
        assert!(
            msg.contains(missing.to_string_lossy().as_ref()),
            "expected error to include the offending path, got: {msg}"
        );
    }

    #[test]
    fn read_sidecar_sync_errors_on_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("garbage.json");
        std::fs::write(&bad, b"{ not valid json").unwrap();
        let err = read_sidecar_sync(&bad).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("failed to parse sidecar"),
            "expected parse-failure prefix, got: {msg}"
        );
        assert!(
            msg.contains(bad.to_string_lossy().as_ref()),
            "expected error to include the offending path, got: {msg}"
        );
    }

    #[tokio::test]
    async fn write_sidecar_at_errors_when_path_has_no_parent() {
        // `Path::new("").parent()` is `None` cross-platform — exercises the
        // early-return guard in `write_sidecar_sync` before any filesystem
        // call is attempted.
        let doc = doc_with_path("doc-1", "/tmp/x.fits");
        let err = write_sidecar_at(Path::new(""), &doc).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("sidecar path has no parent"),
            "expected parent-missing message, got: {msg}"
        );
    }

    #[test]
    fn serialization_skips_none_max_adu() {
        let mut doc = doc_with_path("doc-1", "/tmp/x.fits");
        doc.max_adu = None;
        let body = serde_json::to_string(&doc).unwrap();
        assert!(
            !body.contains("max_adu"),
            "max_adu should be omitted when None, got: {body}"
        );
    }

    #[test]
    fn serialization_skips_none_target_and_frame_type() {
        let doc = doc_with_path("doc-1", "/tmp/x.fits");
        let body = serde_json::to_string(&doc).unwrap();
        let parsed: Value = serde_json::from_str(&body).unwrap();
        let obj = parsed.as_object().expect("top-level should be an object");
        assert!(
            !obj.contains_key("target"),
            "target key should be omitted when None, got: {body}"
        );
        assert!(
            !obj.contains_key("frame_type"),
            "frame_type key should be omitted when None, got: {body}"
        );
    }

    #[test]
    fn exposure_target_from_store_target_denormalizes_every_field() {
        let target = rp_targets::Target {
            slug: rp_targets::TargetSlug::new("m33").unwrap(),
            display_name: "M33".to_string(),
            coord: rp_targets::IcrsCoord::try_new(1.4642, 30.6602).unwrap(),
            catalog_ref: None,
            object_type: None,
            magnitude: None,
            size_arcmin: None,
            position_angle_degrees: None,
            priority: 0,
            active: true,
            goals: Vec::new(),
            scheduling: None,
            grading: None,
            notes: None,
            created_at: String::new(),
            updated_at: String::new(),
            created_by: "operator".to_string(),
            updated_by: "operator".to_string(),
        };
        let exposure_target = ExposureTarget::from(&target);
        assert_eq!(exposure_target.slug, "m33");
        assert_eq!(exposure_target.display_name.as_deref(), Some("M33"));
        assert_eq!(exposure_target.ra_hours, Some(1.4642));
        assert_eq!(exposure_target.dec_degrees, Some(30.6602));
    }

    #[test]
    fn exposure_target_round_trips_through_json_on_the_document() {
        let mut doc = doc_with_path("doc-1", "/tmp/x.fits");
        doc.target = Some(ExposureTarget {
            slug: "dark".to_string(),
            display_name: None,
            ra_hours: None,
            dec_degrees: None,
        });
        doc.frame_type = Some(rp_vocabulary::FrameType::Dark);
        let body = serde_json::to_string(&doc).unwrap();
        let parsed: ExposureDocument = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.target, doc.target);
        assert_eq!(parsed.frame_type, doc.frame_type);
    }

    #[test]
    fn serialization_skips_none_optics() {
        // Parse the JSON and check the top-level key is absent — `contains`
        // would false-pass if a future field name happened to embed
        // "optics" as a substring.
        let mut doc = doc_with_path("doc-1", "/tmp/x.fits");
        doc.optics = None;
        let body = serde_json::to_string(&doc).unwrap();
        let parsed: Value = serde_json::from_str(&body).unwrap();
        let obj = parsed.as_object().expect("top-level should be an object");
        assert!(
            !obj.contains_key("optics"),
            "optics key should be omitted when None, got: {body}"
        );
    }

    fn geometry(
        focal_length_mm: f64,
        pixel_um_x: f64,
        pixel_um_y: f64,
        width_px: u32,
        height_px: u32,
    ) -> CameraGeometry {
        CameraGeometry {
            focal_length_mm,
            pixel_size_x_um: pixel_um_x,
            pixel_size_y_um: pixel_um_y,
            sensor_width_px: width_px,
            sensor_height_px: height_px,
        }
    }

    #[test]
    fn optics_round_trips_through_json() {
        let mut doc = doc_with_path("doc-1", "/tmp/x.fits");
        let optics =
            Optics::from_camera_geometry(geometry(1000.0, 3.76, 3.76, 9576, 6388)).unwrap();
        doc.optics = Some(optics.clone());
        let body = serde_json::to_string(&doc).unwrap();
        let parsed: ExposureDocument = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.optics, Some(optics));
    }

    #[test]
    fn optics_derives_pixel_scale_and_fov() {
        // 1000 mm focal length, 3.76 µm pixels, IMX455-class sensor.
        // Pixel scale = 206.265 × 3.76 / 1000  ≈ 0.775_556_4 arcsec/px
        // Width FOV   = 0.775_556_4 × 9576 / 3600 ≈ 2.062_980 deg
        // Height FOV  = 0.775_556_4 × 6388 / 3600 ≈ 1.376_181 deg
        let optics =
            Optics::from_camera_geometry(geometry(1000.0, 3.76, 3.76, 9576, 6388)).unwrap();
        assert!(
            (optics.pixel_scale_x_arcsec_per_pixel - 0.775_556_4).abs() < 1e-6,
            "pixel_scale_x_arcsec_per_pixel = {}",
            optics.pixel_scale_x_arcsec_per_pixel
        );
        assert!(
            (optics.fov_width_deg - 2.062_980).abs() < 1e-5,
            "fov_width_deg = {}",
            optics.fov_width_deg
        );
        assert!(
            (optics.fov_height_deg - 1.376_181).abs() < 1e-5,
            "fov_height_deg = {}",
            optics.fov_height_deg
        );
    }

    #[test]
    fn optics_supports_anisotropic_pixels() {
        let optics = Optics::from_camera_geometry(geometry(1000.0, 4.0, 5.0, 100, 100)).unwrap();
        assert!(
            optics.pixel_scale_x_arcsec_per_pixel < optics.pixel_scale_y_arcsec_per_pixel,
            "wider pixels in y must produce a larger y pixel scale"
        );
        assert!(optics.fov_width_deg < optics.fov_height_deg);
    }

    #[test]
    fn optics_rejects_non_positive_inputs() {
        assert!(Optics::from_camera_geometry(geometry(0.0, 3.76, 3.76, 1024, 1024)).is_none());
        assert!(Optics::from_camera_geometry(geometry(-1.0, 3.76, 3.76, 1024, 1024)).is_none());
        assert!(Optics::from_camera_geometry(geometry(1000.0, 0.0, 3.76, 1024, 1024)).is_none());
        assert!(Optics::from_camera_geometry(geometry(1000.0, 3.76, -3.76, 1024, 1024)).is_none());
        assert!(Optics::from_camera_geometry(geometry(1000.0, 3.76, 3.76, 0, 1024)).is_none());
        assert!(Optics::from_camera_geometry(geometry(1000.0, 3.76, 3.76, 1024, 0)).is_none());
        assert!(Optics::from_camera_geometry(geometry(f64::NAN, 3.76, 3.76, 1024, 1024)).is_none());
        assert!(
            Optics::from_camera_geometry(geometry(f64::INFINITY, 3.76, 3.76, 1024, 1024)).is_none()
        );
    }

    #[test]
    fn optics_rejects_when_derived_overflows_to_infinity() {
        // A sub-normal focal length passes the input filter (it's finite
        // and > 0) but pushes the derived pixel scale and FOV to `inf`.
        // `serde_json` rejects non-finite floats, so persisting this in
        // the sidecar would fail the entire exposure-document write —
        // breaking capture's persistence contract for an auxiliary block.
        // The constructor must guard against it and return `None`.
        let optics =
            Optics::from_camera_geometry(geometry(f64::MIN_POSITIVE, 3.76, 3.76, 1024, 1024));
        assert!(
            optics.is_none(),
            "derivation must reject inputs that overflow to infinity, got: {optics:?}"
        );
    }

    #[test]
    fn optics_with_overflow_inputs_does_not_break_sidecar_serialization() {
        // Smoke test: even when a hypothetical buggy mock returned wild
        // values, building the document and serializing must succeed —
        // because `from_camera_geometry` returns `None` and the doc's
        // `optics` field is `skip_serializing_if = Option::is_none`.
        let mut doc = doc_with_path("doc-1", "/tmp/x.fits");
        doc.optics = Optics::from_camera_geometry(geometry(f64::MIN_POSITIVE, 1.0, 1.0, 1, 1));
        assert!(doc.optics.is_none());
        serde_json::to_string(&doc).expect("doc must serialize when optics derivation declined");
    }
}

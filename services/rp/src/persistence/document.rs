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

/// The version of `rp` this build is: what a capture records as its
/// document's `rp_version`.
pub const RP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// A captured exposure plus any sections tools have written against it.
///
/// `sections` is an open map keyed by tool/plugin name (`image_analysis`,
/// `flat_calibration`, `plate_solve`, etc.). Each section's shape is owned by
/// its writer; `rp` does not validate them.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExposureDocument {
    pub id: String,
    /// RFC3339 timestamp of capture completion.
    pub captured_at: String,
    /// RFC3339 timestamp (millisecond precision) of exposure *start*:
    /// the camera's `LastExposureStartTime` when it falls inside the
    /// window `rp` observed, else `rp`'s own clock reading taken as
    /// `StartExposure` returned — see `docs/services/rp.md` §"Core
    /// Fields". The FITS header's `DATE-OBS`. Present on every frame
    /// this version captures; absent on older sidecars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exposure_started_at: Option<String>,
    /// Absolute path to the FITS file on disk.
    pub file_path: String,
    pub width: u32,
    pub height: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_id: Option<String>,
    /// The driver's ASCOM `Name` for the capturing camera, read once per
    /// connection. The FITS header's `INSTRUME`. Omitted when the
    /// connect-time read failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_name: Option<String>,
    /// The optical train that terminates in the capturing camera.
    /// Omitted for a camera outside every train.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub train_id: Option<String>,
    /// That train's configured telescope (`optical_trains[].telescope`)
    /// at capture time — the FITS header's `TELESCOP`. Omitted when the
    /// camera is outside every train or its train names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telescope: Option<String>,
    /// That train's devices in optical order, objective side first, as
    /// the config stood at capture time — `get_train_info`'s `devices`.
    /// Omitted for a camera outside every train.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub train_devices: Option<Vec<ExposureTrainDevice>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "humantime_serde"
    )]
    pub duration: Option<Duration>,
    /// The binning the frame was read out at, read back off the camera
    /// rather than copied from the request. A camera that lands on a
    /// different binning than it was set to fails the capture before
    /// exposing, so no document is written for it — the read-back is
    /// what makes that check possible, and what this field records.
    /// The one case where it can still misname a frame is a client
    /// outside `rp` re-binning the camera after the read-back; `rp`'s
    /// own captures through one camera run one at a time (rp.md
    /// §"Capture Tool Details", "Binning" → Concurrency). Present on
    /// every frame this version captures;
    /// absent on sidecars written before `capture` set the binning at
    /// all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binning: Option<rp_vocabulary::Binning>,
    /// The filter in the camera's train filter wheel during the
    /// exposure. Omitted for `Dark`/`Bias` frames, for a train without
    /// a wheel, and when the wheel read failed — see
    /// `docs/services/rp.md` §"Core Fields".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// The wheel's zero-based `Position` from the read that named
    /// `filter`; present exactly when `filter` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_position: Option<u32>,
    /// That slot's configured `wavelength_nm` at capture time. Omitted
    /// with `filter`, and when the slot's entry carries no wavelength.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_wavelength_nm: Option<f64>,
    /// The camera's ASCOM `Gain`, read on every capture. Omitted when
    /// the driver does not implement it or the read failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gain: Option<i32>,
    /// The camera's ASCOM `Offset`, under the same rule as `gain`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<i32>,
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
    /// Where the mount pointed during the exposure, read once as the
    /// exposure started. Omitted when no mount is connected or the read
    /// failed. The FITS header's `RA`/`DEC`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointing: Option<MountPointing>,
    /// The configured observer site at capture time — the FITS
    /// header's `SITELAT`/`SITELONG`. Omitted when no site is
    /// configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<ExposureSite>,
    /// The version of `rp` that captured the frame — the FITS header's
    /// `SWCREATE`. Present on every frame this version captures; a
    /// later `rp` rewriting the sidecar keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rp_version: Option<String>,
    /// Optical-train geometry resolved at capture time: the train's
    /// configured facts, the camera's connect-time reads and the
    /// terminal focuser's step size, each recorded on its own when it
    /// is usable, plus the pixel scale and FOV derived from them, so a
    /// consumer, such as a caller passing `plate_solve`'s
    /// `fov_hint_deg`, need not re-derive them from the FITS header.
    /// Omitted only when nothing in it is known — see
    /// `docs/services/rp.md` §"Core Fields".
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
    /// This capture's `light_source` parameter, verbatim: what lit the
    /// frame. Omitted when the caller gave none — see
    /// `docs/services/rp.md` §"Core Fields".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub light_source: Option<LightSource>,
    #[serde(default)]
    pub sections: Map<String, Value>,
}

/// What lights a frame: the `capture` parameter that tells a panel flat
/// from a sky flat, both of which are `FrameType::Flat`.
///
/// While conditions are unsafe, a capture is taken only when it does
/// not need the sky, and `Panel` is how a lit frame says so
/// (`docs/services/rp.md` §"Capture Tool Details", *Light source*).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LightSource {
    /// A flat panel or another light source behind closed optics.
    Panel,
    /// The sky, as for a twilight flat.
    Sky,
}

/// The exposure document's `target` field (Decision 11).
///
/// `display_name`/`ra_hours`/`dec_degrees` are populated only when
/// `slug` resolved against a real target-store row — `None` for a
/// `Dark`/`Flat`/`Bias` capture's reserved slug, which names no store
/// entry.
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

/// The exposure document's `pointing` field: where the mount pointed.
///
/// `RightAscension`/`Declination` exactly as the mount reported them, in
/// its own equatorial system (`rp` does not convert) — distinct from
/// [`ExposureTarget`]'s catalog coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MountPointing {
    pub ra_hours: f64,
    pub dec_degrees: f64,
}

/// One entry of the exposure document's `train_devices`: a roster id
/// and the kind it resolved to, spelled as `get_train_info` spells it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExposureTrainDevice {
    pub id: String,
    pub kind: String,
}

/// The exposure document's `site` field: the configured observer site.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExposureSite {
    pub latitude_degrees: f64,
    pub longitude_degrees: f64,
}

impl From<&rp_ephemeris::Site> for ExposureSite {
    fn from(site: &rp_ephemeris::Site) -> Self {
        Self {
            latitude_degrees: site.latitude_degrees,
            longitude_degrees: site.longitude_degrees,
        }
    }
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
/// time.
///
/// Every field is recorded on its own: an input when it is known and
/// usable, a derived value when all its inputs are and it is itself
/// usable. See [`ExposureDocument::optics`] and `docs/services/rp.md`
/// §"Core Fields" for the sources, the rules, and the `plate_solve`
/// consumer contract.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Optics {
    /// Operator-supplied effective focal length of the optical train,
    /// in millimetres, copied at capture time from the camera's train
    /// (`equipment.optical_trains[].focal_length_mm`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focal_length_mm: Option<f64>,
    /// The same train's clear aperture in millimetres
    /// (`optical_trains[].aperture_mm`); with the focal length, the
    /// frame's focal ratio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aperture_mm: Option<f64>,
    /// Raw Alpaca `PixelSizeX` reading, in microns, read when the camera
    /// session was established and cached for the connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pixel_size_x_um: Option<f64>,
    /// Raw Alpaca `PixelSizeY` reading, in microns, cached like
    /// `pixel_size_x_um`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pixel_size_y_um: Option<f64>,
    /// Raw Alpaca `CameraXSize` reading, in pixels, cached like
    /// `pixel_size_x_um`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensor_width_px: Option<u32>,
    /// Raw Alpaca `CameraYSize` reading, in pixels, cached like
    /// `pixel_size_x_um`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensor_height_px: Option<u32>,
    /// The image-plane travel of one step of the train's terminal
    /// focuser, in microns: its configured `microns_per_step`, else the
    /// driver's `StepSize` (rp.md § Train optics).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microns_per_step: Option<f64>,
    /// Derived: `206.265 × pixel_size_x_um / focal_length_mm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pixel_scale_x_arcsec_per_pixel: Option<f64>,
    /// Derived: `206.265 × pixel_size_y_um / focal_length_mm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pixel_scale_y_arcsec_per_pixel: Option<f64>,
    /// Derived: `pixel_scale_x_arcsec_per_pixel × sensor_width_px / 3600`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fov_width_deg: Option<f64>,
    /// Derived: `pixel_scale_y_arcsec_per_pixel × sensor_height_px / 3600`.
    /// Matches ASTAP's `-fov` semantics ("image height in degrees"), so
    /// it is the source rp.md §"Ground rules" names for `plate_solve`'s
    /// `fov_hint_deg`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fov_height_deg: Option<f64>,
}

/// The raw inputs [`Optics::from_inputs`] records and derives from.
///
/// The train's configured facts, the camera-reported pixel size and
/// sensor dimensions, and the terminal focuser's step size, each
/// `None` when unknown.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpticsInputs {
    pub focal_length_mm: Option<f64>,
    pub aperture_mm: Option<f64>,
    pub pixel_size_x_um: Option<f64>,
    pub pixel_size_y_um: Option<f64>,
    pub sensor_width_px: Option<u32>,
    pub sensor_height_px: Option<u32>,
    pub microns_per_step: Option<f64>,
}

/// Arcseconds per radian over a thousand, so that microns over
/// millimetres come out in arcseconds.
const ARCSEC_PER_MM_PER_UM: f64 = 206.265;

impl Optics {
    /// Record each usable input and derive what the usable inputs
    /// allow. A real is usable when it is finite and positive, a pixel
    /// count when it is above zero; anything else is left out without
    /// taking the other fields with it. `None` when nothing is left.
    ///
    /// Derived values are validated too because `serde_json` rejects
    /// non-finite floats: a sub-normal `focal_length_mm` could push the
    /// pixel scale to `inf`, which would then fail the *entire* sidecar
    /// write — breaking capture's persistence contract for an auxiliary
    /// metadata block. Defense in depth keeps the failure scoped to the
    /// one derived field.
    #[must_use]
    pub fn from_inputs(inputs: OpticsInputs) -> Option<Self> {
        let focal_length_mm = usable(inputs.focal_length_mm);
        let width = AxisOptics::new(
            inputs.pixel_size_x_um,
            inputs.sensor_width_px,
            focal_length_mm,
        );
        let height = AxisOptics::new(
            inputs.pixel_size_y_um,
            inputs.sensor_height_px,
            focal_length_mm,
        );
        let optics = Self {
            focal_length_mm,
            aperture_mm: usable(inputs.aperture_mm),
            pixel_size_x_um: width.pixel_size_um,
            pixel_size_y_um: height.pixel_size_um,
            sensor_width_px: width.sensor_px,
            sensor_height_px: height.sensor_px,
            microns_per_step: usable(inputs.microns_per_step),
            pixel_scale_x_arcsec_per_pixel: width.pixel_scale,
            pixel_scale_y_arcsec_per_pixel: height.pixel_scale,
            fov_width_deg: width.fov_deg,
            fov_height_deg: height.fov_deg,
        };
        (optics != Self::default()).then_some(optics)
    }

    /// The frame's focal ratio, `focal_length_mm / aperture_mm`, as
    /// `get_train_info` derives it; `None` unless both are recorded.
    #[must_use]
    pub fn focal_ratio(&self) -> Option<f64> {
        match (self.focal_length_mm, self.aperture_mm) {
            (Some(focal_length), Some(aperture)) => usable(Some(focal_length / aperture)),
            _ => None,
        }
    }
}

/// One sensor axis of the optics block: its usable pixel size and
/// sensor size, and what they derive with the focal length.
struct AxisOptics {
    pixel_size_um: Option<f64>,
    sensor_px: Option<u32>,
    pixel_scale: Option<f64>,
    fov_deg: Option<f64>,
}

impl AxisOptics {
    fn new(
        pixel_size_um: Option<f64>,
        sensor_px: Option<u32>,
        focal_length_mm: Option<f64>,
    ) -> Self {
        let pixel_size_um = usable(pixel_size_um);
        let sensor_px = sensor_px.filter(|px| *px > 0);
        let pixel_scale = match (pixel_size_um, focal_length_mm) {
            (Some(um), Some(mm)) => usable(Some(ARCSEC_PER_MM_PER_UM * um / mm)),
            _ => None,
        };
        let fov_deg = match (pixel_scale, sensor_px) {
            (Some(scale), Some(px)) => usable(Some(scale * f64::from(px) / 3600.0)),
            _ => None,
        };
        Self {
            pixel_size_um,
            sensor_px,
            pixel_scale,
            fov_deg,
        }
    }
}

/// A real the optics block can record: finite and positive.
fn usable(value: Option<f64>) -> Option<f64> {
    value.filter(|v| v.is_finite() && *v > 0.0)
}

impl ExposureDocument {
    /// Sidecar JSON path for a given FITS file path (`/foo/<uuid8>.fits` →
    /// `/foo/<uuid8>.json`). Returns `None` when the path doesn't end in
    /// `.fits` — case-sensitive, mirroring the capture-side filename
    /// convention. The guard matters because `with_extension("json")` maps
    /// a `.json` path to itself, so an unvalidated derivation would let a
    /// sidecar write clobber its own input file.
    #[must_use]
    pub fn sidecar_path_for(fits_path: impl AsRef<Path>) -> Option<PathBuf> {
        let path = fits_path.as_ref();
        if path.extension().and_then(|e| e.to_str()) != Some("fits") {
            return None;
        }
        Some(path.with_extension("json"))
    }

    /// Read a sidecar JSON file from disk and deserialize it. Synchronous
    /// because sidecars are small (single-digit KB even with measurement
    /// sections); the disk-fallback resolver runs the whole scan on the
    /// blocking pool already.
    ///
    /// # Errors
    ///
    /// Returns [`RpError::Imaging`] if the file cannot be read or is not
    /// a valid document.
    pub fn read_sidecar_sync(path: &Path) -> Result<Self> {
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

    /// Atomically write this document to its sidecar JSON path
    /// ([`sidecar_path_for`](Self::sidecar_path_for) applied to
    /// `file_path`).
    ///
    /// Stages into a sibling temp file, fsyncs, renames into place, fsyncs
    /// the parent directory (unix-only). Mirrors the FITS write treatment
    /// in `imaging::fits::write_fits`.
    ///
    /// # Errors
    ///
    /// Returns [`RpError::Imaging`] if `file_path` doesn't end in
    /// `.fits`; otherwise [`write_sidecar_at`](Self::write_sidecar_at)'s
    /// errors.
    pub async fn write_sidecar(&self) -> Result<()> {
        let path = Self::sidecar_path_for(&self.file_path).ok_or_else(|| {
            RpError::Imaging(format!(
                "cannot derive sidecar path: file_path is not a .fits path: {}",
                self.file_path
            ))
        })?;
        self.write_sidecar_at(&path).await
    }

    /// As [`write_sidecar`](Self::write_sidecar), but writes to an explicit
    /// path. Used by tests that want to verify sidecar I/O without
    /// round-tripping through the rest of the pipeline.
    ///
    /// # Errors
    ///
    /// Returns [`RpError::Json`] if the document fails to serialize,
    /// [`RpError::Imaging`] if `path` has no parent directory or the
    /// blocking write task fails to join, and [`RpError::Io`] if any
    /// step of the staged write fails — creating the parent dirs, the
    /// temp file, writing, fsyncing, the rename into place, or the
    /// post-rename directory fsync (Unix).
    pub async fn write_sidecar_at(&self, path: &Path) -> Result<()> {
        let body = serde_json::to_vec_pretty(self)?;
        let path = path.to_path_buf();
        // Run the whole stage-and-commit sequence on the blocking pool. Matches
        // the `imaging::fits::write_fits` pattern: one task spawn per write rather
        // than one per `tokio::fs::*` call, and lets us use sync-only crates like
        // `tempfile` for the staging file.
        tokio::task::spawn_blocking(move || write_sidecar_sync(&path, &body))
            .await
            .map_err(|e| RpError::Imaging(format!("sidecar write task join error: {e}")))?
    }
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
            binning: None,
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
            ..ExposureDocument::default()
        }
    }

    #[tokio::test]
    async fn write_sidecar_round_trips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let fits_path = dir.path().join("img.fits").to_string_lossy().into_owned();
        let sidecar = dir.path().join("img.json");

        let doc = doc_with_path("doc-1", &fits_path);
        doc.write_sidecar().await.unwrap();

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
        doc.write_sidecar().await.unwrap();

        doc.duration = Some(Duration::from_secs(2));
        doc.write_sidecar().await.unwrap();

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
        doc.write_sidecar().await.unwrap();

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
        let err = doc.write_sidecar().await.unwrap_err();
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
    fn sidecar_path_for_swaps_fits_extension() {
        assert_eq!(
            ExposureDocument::sidecar_path_for("/data/lights/550e8400.fits").unwrap(),
            std::path::PathBuf::from("/data/lights/550e8400.json")
        );
    }

    #[test]
    fn sidecar_path_for_rejects_non_fits_extensions() {
        assert!(ExposureDocument::sidecar_path_for("/tmp/x.json").is_none());
        assert!(ExposureDocument::sidecar_path_for("/tmp/x.txt").is_none());
        assert!(ExposureDocument::sidecar_path_for("/tmp/x").is_none());
        assert!(ExposureDocument::sidecar_path_for("/tmp/x.FITS").is_none());
    }

    #[tokio::test]
    async fn write_sidecar_errors_when_file_path_is_not_fits() {
        let doc = doc_with_path("doc-1", "/tmp/x.json");
        let err = doc.write_sidecar().await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not a .fits path"),
            "expected derivation-failure message, got: {msg}"
        );
    }

    #[test]
    fn read_sidecar_sync_errors_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does_not_exist.json");
        let err = ExposureDocument::read_sidecar_sync(&missing).unwrap_err();
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
        let err = ExposureDocument::read_sidecar_sync(&bad).unwrap_err();
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
        let err = doc.write_sidecar_at(Path::new("")).await.unwrap_err();
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
        assert!(
            !obj.contains_key("light_source"),
            "light_source key should be omitted when None, got: {body}"
        );
    }

    /// The sidecar spells the light source as the `capture` parameter
    /// does, lowercase.
    #[test]
    fn light_source_round_trips_in_its_lowercase_spelling() {
        for (light_source, spelling) in [(LightSource::Panel, "panel"), (LightSource::Sky, "sky")] {
            let mut doc = doc_with_path("doc-1", "/tmp/x.fits");
            doc.light_source = Some(light_source);
            let body: Value = serde_json::to_value(&doc).unwrap();
            assert_eq!(body["light_source"], spelling);
            let parsed: ExposureDocument = serde_json::from_value(body).unwrap();
            assert_eq!(parsed.light_source, Some(light_source));
        }
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

    /// A complete set of inputs: 1000 mm at 200 mm aperture, 3.76 µm
    /// pixels on an IMX455-class sensor, a 2.5 µm focuser step.
    const fn inputs() -> OpticsInputs {
        OpticsInputs {
            focal_length_mm: Some(1000.0),
            aperture_mm: Some(200.0),
            pixel_size_x_um: Some(3.76),
            pixel_size_y_um: Some(3.76),
            sensor_width_px: Some(9576),
            sensor_height_px: Some(6388),
            microns_per_step: Some(2.5),
        }
    }

    #[test]
    fn optics_round_trips_through_json() {
        let mut doc = doc_with_path("doc-1", "/tmp/x.fits");
        let optics = Optics::from_inputs(inputs()).unwrap();
        doc.optics = Some(optics.clone());
        let body = serde_json::to_string(&doc).unwrap();
        let parsed: ExposureDocument = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.optics, Some(optics));
    }

    #[test]
    fn a_complete_optics_block_without_the_newer_fields_still_parses() {
        let body = serde_json::json!({
            "focal_length_mm": 1000.0,
            "pixel_size_x_um": 3.76,
            "pixel_size_y_um": 3.76,
            "sensor_width_px": 9576,
            "sensor_height_px": 6388,
            "pixel_scale_x_arcsec_per_pixel": 0.7756,
            "pixel_scale_y_arcsec_per_pixel": 0.7756,
            "fov_width_deg": 2.063,
            "fov_height_deg": 1.3762
        });
        let optics: Optics = serde_json::from_value(body).unwrap();
        assert_eq!(optics.focal_length_mm, Some(1000.0));
        assert_eq!(optics.fov_height_deg, Some(1.3762));
        assert_eq!(optics.aperture_mm, None);
        assert_eq!(optics.microns_per_step, None);
    }

    #[test]
    fn optics_records_the_inputs_as_given() {
        let optics = Optics::from_inputs(inputs()).unwrap();
        assert_eq!(optics.focal_length_mm, Some(1000.0));
        assert_eq!(optics.aperture_mm, Some(200.0));
        assert_eq!(optics.pixel_size_x_um, Some(3.76));
        assert_eq!(optics.pixel_size_y_um, Some(3.76));
        assert_eq!(optics.sensor_width_px, Some(9576));
        assert_eq!(optics.sensor_height_px, Some(6388));
        assert_eq!(optics.microns_per_step, Some(2.5));
    }

    #[test]
    fn optics_derives_pixel_scale_and_fov() {
        // Pixel scale = 206.265 × 3.76 / 1000  ≈ 0.775_556_4 arcsec/px
        // Width FOV   = 0.775_556_4 × 9576 / 3600 ≈ 2.062_980 deg
        // Height FOV  = 0.775_556_4 × 6388 / 3600 ≈ 1.376_181 deg
        let optics = Optics::from_inputs(inputs()).unwrap();
        let scale_x = optics.pixel_scale_x_arcsec_per_pixel.unwrap();
        let fov_width = optics.fov_width_deg.unwrap();
        let fov_height = optics.fov_height_deg.unwrap();
        assert!((scale_x - 0.775_556_4).abs() < 1e-6, "scale x = {scale_x}");
        assert!(
            (fov_width - 2.062_980).abs() < 1e-5,
            "fov width = {fov_width}"
        );
        assert!(
            (fov_height - 1.376_181).abs() < 1e-5,
            "fov height = {fov_height}"
        );
    }

    #[test]
    fn optics_supports_anisotropic_pixels() {
        let optics = Optics::from_inputs(OpticsInputs {
            pixel_size_x_um: Some(4.0),
            pixel_size_y_um: Some(5.0),
            sensor_width_px: Some(100),
            sensor_height_px: Some(100),
            ..inputs()
        })
        .unwrap();
        assert!(
            optics.pixel_scale_x_arcsec_per_pixel < optics.pixel_scale_y_arcsec_per_pixel,
            "wider pixels in y must produce a larger y pixel scale"
        );
        assert!(optics.fov_width_deg < optics.fov_height_deg);
    }

    #[test]
    fn an_unusable_pixel_size_drops_only_that_axis() {
        let optics = Optics::from_inputs(OpticsInputs {
            pixel_size_x_um: Some(0.0),
            ..inputs()
        })
        .unwrap();
        assert_eq!(optics.pixel_size_x_um, None);
        assert_eq!(optics.pixel_scale_x_arcsec_per_pixel, None);
        assert_eq!(optics.fov_width_deg, None);
        assert_eq!(optics.focal_length_mm, Some(1000.0));
        assert_eq!(optics.sensor_width_px, Some(9576));
        assert_eq!(optics.pixel_size_y_um, Some(3.76));
        assert!(optics.fov_height_deg.is_some());
    }

    #[test]
    fn without_a_focal_length_the_camera_geometry_is_kept_and_nothing_derived() {
        let optics = Optics::from_inputs(OpticsInputs {
            focal_length_mm: None,
            aperture_mm: None,
            microns_per_step: None,
            ..inputs()
        })
        .unwrap();
        assert_eq!(
            optics,
            Optics {
                pixel_size_x_um: Some(3.76),
                pixel_size_y_um: Some(3.76),
                sensor_width_px: Some(9576),
                sensor_height_px: Some(6388),
                ..Optics::default()
            }
        );
    }

    #[test]
    fn a_missing_sensor_size_keeps_the_pixel_scale_and_drops_the_fov() {
        let optics = Optics::from_inputs(OpticsInputs {
            sensor_height_px: Some(0),
            ..inputs()
        })
        .unwrap();
        assert_eq!(optics.sensor_height_px, None);
        assert_eq!(optics.fov_height_deg, None);
        assert!(optics.pixel_scale_y_arcsec_per_pixel.is_some());
        assert!(optics.fov_width_deg.is_some());
    }

    #[test]
    fn each_unusable_real_is_left_out() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let optics = Optics::from_inputs(OpticsInputs {
                focal_length_mm: Some(bad),
                aperture_mm: Some(bad),
                pixel_size_x_um: Some(bad),
                pixel_size_y_um: Some(bad),
                microns_per_step: Some(bad),
                ..inputs()
            })
            .unwrap();
            assert_eq!(
                optics,
                Optics {
                    sensor_width_px: Some(9576),
                    sensor_height_px: Some(6388),
                    ..Optics::default()
                },
                "for {bad}"
            );
        }
    }

    #[test]
    fn nothing_known_gives_no_block() {
        assert_eq!(Optics::from_inputs(OpticsInputs::default()), None);
        let all_unusable = OpticsInputs {
            focal_length_mm: Some(0.0),
            sensor_width_px: Some(0),
            ..OpticsInputs::default()
        };
        assert_eq!(Optics::from_inputs(all_unusable), None);
    }

    #[test]
    fn a_derivation_that_overflows_is_left_out() {
        // A sub-normal focal length passes the input filter (it's finite
        // and > 0) but pushes the derived pixel scale and FOV to `inf`.
        // `serde_json` rejects non-finite floats, so persisting this in
        // the sidecar would fail the entire exposure-document write —
        // breaking capture's persistence contract for an auxiliary block.
        let optics = Optics::from_inputs(OpticsInputs {
            focal_length_mm: Some(f64::MIN_POSITIVE),
            ..inputs()
        })
        .unwrap();
        assert_eq!(optics.focal_length_mm, Some(f64::MIN_POSITIVE));
        assert_eq!(optics.pixel_scale_x_arcsec_per_pixel, None);
        assert_eq!(optics.pixel_scale_y_arcsec_per_pixel, None);
        assert_eq!(optics.fov_width_deg, None);
        assert_eq!(optics.fov_height_deg, None);
        let mut doc = doc_with_path("doc-1", "/tmp/x.fits");
        doc.optics = Some(optics);
        serde_json::to_string(&doc).expect("doc must serialize when a derivation is left out");
    }

    #[test]
    fn a_partial_block_serializes_without_nulls() {
        let optics = Optics::from_inputs(OpticsInputs {
            pixel_size_x_um: Some(3.76),
            ..OpticsInputs::default()
        })
        .unwrap();
        assert_eq!(
            serde_json::to_value(&optics).unwrap(),
            serde_json::json!({ "pixel_size_x_um": 3.76 })
        );
    }

    #[test]
    fn the_focal_ratio_needs_both_focal_length_and_aperture() {
        let optics = Optics::from_inputs(inputs()).unwrap();
        assert_eq!(optics.focal_ratio(), Some(5.0));
        let no_aperture = Optics {
            aperture_mm: None,
            ..optics
        };
        assert_eq!(no_aperture.focal_ratio(), None);
    }

    #[test]
    fn the_train_and_site_facts_round_trip_through_json() {
        let mut doc = doc_with_path("doc-1", "/tmp/x.fits");
        doc.train_devices = Some(vec![
            ExposureTrainDevice {
                id: "main-fw".to_string(),
                kind: "filter wheel".to_string(),
            },
            ExposureTrainDevice {
                id: "main-cam".to_string(),
                kind: "camera".to_string(),
            },
        ]);
        doc.filter_position = Some(3);
        doc.filter_wavelength_nm = Some(656.3);
        doc.site = Some(ExposureSite {
            latitude_degrees: 51.0786,
            longitude_degrees: -0.2944,
        });
        doc.rp_version = Some(RP_VERSION.to_string());
        let body = serde_json::to_string(&doc).unwrap();
        let parsed: ExposureDocument = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.train_devices, doc.train_devices);
        assert_eq!(parsed.filter_position, Some(3));
        assert_eq!(parsed.filter_wavelength_nm, Some(656.3));
        assert_eq!(parsed.site, doc.site);
        assert_eq!(parsed.rp_version.as_deref(), Some(RP_VERSION));
    }
}

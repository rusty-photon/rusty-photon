//! The calibration store and a calibration's record on the wire.
//!
//! A calibration made within 60° of the celestial equator is kept in
//! `calibration.json` in the state directory, in the slot of its pier side
//! (docs/services/guider.md § Calibration, "The store"). The file is written
//! whole, through a temporary file that then takes its name. A file the
//! guider cannot read is set aside when the service starts, and the store
//! starts empty.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;
use rp_guiding::{
    Axis, Calibration, CameraGeometry, Check, DecAxis, GuideRates, Leg, PierSide, Setup, Warning,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// The store's file in the state directory.
const FILE: &str = "calibration.json";

/// A leg as the API names it.
#[must_use]
pub const fn leg_name(leg: Leg) -> &'static str {
    match leg {
        Leg::West => "west",
        Leg::East => "east",
        Leg::ClearBacklash => "clear_backlash",
        Leg::North => "north",
        Leg::South => "south",
    }
}

/// A cross-check as the API names it.
#[must_use]
pub const fn check_name(check: Check) -> &'static str {
    match check {
        Check::Orthogonality => "orthogonality",
        Check::RaRate => "ra_rate",
        Check::DecRate => "dec_rate",
        Check::RateRatio => "rate_ratio",
    }
}

/// A cross-check named by the API.
fn check_named(name: &str) -> Option<Check> {
    [
        Check::Orthogonality,
        Check::RaRate,
        Check::DecRate,
        Check::RateRatio,
    ]
    .into_iter()
    .find(|check| check_name(*check) == name)
}

/// A pier side as the API names it.
#[must_use]
pub const fn side_name(side: PierSide) -> &'static str {
    match side {
        PierSide::East => "east",
        PierSide::West => "west",
        PierSide::Unknown => "unknown",
    }
}

/// A calibration and when it was made.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub calibrated_at: SystemTime,
    pub calibration: Calibration,
}

/// A record on the wire and in the file (guider.md § `POST
/// /api/v1/calibrate`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordBody {
    calibrated_at: String,
    pier_side: String,
    declination_deg: f64,
    pixel_scale_arcsec: f64,
    focal_length_mm: f64,
    camera: CameraBody,
    guide_rates: Option<GuideRatesBody>,
    ra: AxisBody,
    dec: Option<DecBody>,
    orthogonality_error_deg: Option<f64>,
    expected: Option<ExpectedBody>,
    warnings: Vec<WarningBody>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CameraBody {
    pixel_size_um: f64,
    width: u32,
    height: u32,
    binning: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuideRatesBody {
    ra_deg_per_s: f64,
    dec_deg_per_s: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AxisBody {
    angle_deg: f64,
    rate_px_per_s: f64,
    steps: u32,
    step: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecBody {
    angle_deg: f64,
    rate_px_per_s: f64,
    steps: u32,
    step: String,
    backlash: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedBody {
    ra_rate_px_per_s: f64,
    dec_rate_px_per_s: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WarningBody {
    check: String,
    value: f64,
    threshold: f64,
}

/// A duration as the record writes it: humantime, in whole milliseconds.
fn duration_text(duration: Duration) -> String {
    let millis = Duration::from_millis(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
    humantime::format_duration(millis).to_string()
}

impl From<&Record> for RecordBody {
    fn from(record: &Record) -> Self {
        let calibration = &record.calibration;
        let setup = &calibration.setup;
        let axis = |axis: &Axis| AxisBody {
            angle_deg: axis.angle_deg,
            rate_px_per_s: axis.rate_px_per_s,
            steps: axis.steps,
            step: duration_text(axis.step),
        };
        Self {
            calibrated_at: humantime::format_rfc3339_seconds(record.calibrated_at).to_string(),
            pier_side: side_name(setup.pier_side).to_owned(),
            declination_deg: calibration.declination_deg,
            pixel_scale_arcsec: setup.pixel_scale_arcsec(),
            focal_length_mm: setup.focal_length_mm,
            camera: CameraBody {
                pixel_size_um: setup.camera.pixel_size_um,
                width: setup.camera.width,
                height: setup.camera.height,
                binning: setup.camera.binning,
            },
            guide_rates: setup.guide_rates.map(|rates| GuideRatesBody {
                ra_deg_per_s: rates.ra_deg_per_s,
                dec_deg_per_s: rates.dec_deg_per_s,
            }),
            ra: axis(&calibration.ra),
            dec: calibration.dec.map(|dec| DecBody {
                angle_deg: dec.axis.angle_deg,
                rate_px_per_s: dec.axis.rate_px_per_s,
                steps: dec.axis.steps,
                step: duration_text(dec.axis.step),
                backlash: duration_text(dec.backlash),
            }),
            orthogonality_error_deg: calibration.orthogonality_error_deg(),
            expected: setup.expected_rates().map(|expected| ExpectedBody {
                ra_rate_px_per_s: expected.ra_px_per_s,
                dec_rate_px_per_s: expected.dec_px_per_s,
            }),
            warnings: calibration
                .warnings
                .iter()
                .map(|warning| WarningBody {
                    check: check_name(warning.check).to_owned(),
                    value: warning.value,
                    threshold: warning.threshold,
                })
                .collect(),
        }
    }
}

impl TryFrom<RecordBody> for Record {
    type Error = String;

    fn try_from(body: RecordBody) -> Result<Self, String> {
        let duration = |text: &str| {
            humantime::parse_duration(text).map_err(|e| format!("the duration {text:?}: {e}"))
        };
        let pier_side = [PierSide::East, PierSide::West, PierSide::Unknown]
            .into_iter()
            .find(|side| side_name(*side) == body.pier_side)
            .ok_or_else(|| format!("the pier side {:?}", body.pier_side))?;
        let calibrated_at = humantime::parse_rfc3339(&body.calibrated_at)
            .map_err(|e| format!("the time {:?}: {e}", body.calibrated_at))?;
        let warnings = body
            .warnings
            .iter()
            .map(|warning| {
                Ok(Warning {
                    check: check_named(&warning.check)
                        .ok_or_else(|| format!("the check {:?}", warning.check))?,
                    value: warning.value,
                    threshold: warning.threshold,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let dec = body
            .dec
            .map(|dec| {
                Ok::<_, String>(DecAxis {
                    axis: Axis {
                        angle_deg: dec.angle_deg,
                        rate_px_per_s: dec.rate_px_per_s,
                        steps: dec.steps,
                        step: duration(&dec.step)?,
                    },
                    backlash: duration(&dec.backlash)?,
                })
            })
            .transpose()?;
        Ok(Self {
            calibrated_at,
            calibration: Calibration {
                setup: Setup {
                    pier_side,
                    camera: CameraGeometry {
                        pixel_size_um: body.camera.pixel_size_um,
                        width: body.camera.width,
                        height: body.camera.height,
                        binning: body.camera.binning,
                    },
                    focal_length_mm: body.focal_length_mm,
                    guide_rates: body.guide_rates.map(|rates| GuideRates {
                        ra_deg_per_s: rates.ra_deg_per_s,
                        dec_deg_per_s: rates.dec_deg_per_s,
                    }),
                },
                declination_deg: body.declination_deg,
                ra: Axis {
                    angle_deg: body.ra.angle_deg,
                    rate_px_per_s: body.ra.rate_px_per_s,
                    steps: body.ra.steps,
                    step: duration(&body.ra.step)?,
                },
                dec,
                warnings,
            },
        })
    }
}

/// The store's slots, as the file and `GET /api/v1/calibration` hold them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreBody {
    east: Option<RecordBody>,
    west: Option<RecordBody>,
    unknown: Option<RecordBody>,
}

/// One record a slot for each pier side.
#[derive(Debug, Clone, Default, PartialEq)]
struct Slots {
    east: Option<Record>,
    west: Option<Record>,
    unknown: Option<Record>,
}

impl Slots {
    const fn get(&self, side: PierSide) -> Option<&Record> {
        match side {
            PierSide::East => self.east.as_ref(),
            PierSide::West => self.west.as_ref(),
            PierSide::Unknown => self.unknown.as_ref(),
        }
    }

    const fn slot(&mut self, side: PierSide) -> &mut Option<Record> {
        match side {
            PierSide::East => &mut self.east,
            PierSide::West => &mut self.west,
            PierSide::Unknown => &mut self.unknown,
        }
    }

    fn body(&self) -> StoreBody {
        StoreBody {
            east: self.east.as_ref().map(RecordBody::from),
            west: self.west.as_ref().map(RecordBody::from),
            unknown: self.unknown.as_ref().map(RecordBody::from),
        }
    }
}

impl TryFrom<StoreBody> for Slots {
    type Error = String;

    fn try_from(body: StoreBody) -> Result<Self, String> {
        let record = |body: Option<RecordBody>| body.map(Record::try_from).transpose();
        Ok(Self {
            east: record(body.east)?,
            west: record(body.west)?,
            unknown: record(body.unknown)?,
        })
    }
}

/// The calibration store.
#[derive(Debug)]
pub struct CalibrationStore {
    path: PathBuf,
    slots: Mutex<Slots>,
}

impl CalibrationStore {
    /// The store of the state directory `dir`: what its file holds, or
    /// nothing when it has none, or one the guider cannot read, which is
    /// set aside first.
    #[must_use]
    pub fn open(dir: &Path) -> Self {
        let path = dir.join(FILE);
        let slots = match std::fs::read(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Slots::default(),
            Err(error) => set_aside(&path, &error.to_string()),
            Ok(bytes) => match serde_json::from_slice::<StoreBody>(&bytes)
                .map_err(|e| e.to_string())
                .and_then(Slots::try_from)
            {
                Ok(slots) => {
                    debug!(path = %path.display(), "calibration store read");
                    slots
                }
                Err(error) => set_aside(&path, &error),
            },
        };
        Self {
            path,
            slots: Mutex::new(slots),
        }
    }

    /// The store, as `GET /api/v1/calibration` answers it.
    #[must_use]
    pub fn body(&self) -> StoreBody {
        self.slots.lock().body()
    }

    /// The record in the slot of `side`.
    #[must_use]
    pub fn get(&self, side: PierSide) -> Option<Record> {
        self.slots.lock().get(side).cloned()
    }

    /// Keep `record` in the slot of its pier side, when it is storable.
    ///
    /// # Errors
    ///
    /// A message when the file cannot be written; the slot then keeps what
    /// it held.
    pub fn put(&self, record: Record) -> Result<(), String> {
        if !record.calibration.storable() {
            debug!(
                declination_deg = record.calibration.declination_deg,
                "calibration made more than 60° from the celestial equator: not stored"
            );
            return Ok(());
        }
        let side = record.calibration.setup.pier_side;
        let mut slots = self.slots.lock();
        let mut next = slots.clone();
        *next.slot(side) = Some(record);
        self.write(&next)?;
        *slots = next;
        drop(slots);
        Ok(())
    }

    /// Empty every slot.
    ///
    /// # Errors
    ///
    /// A message when the file cannot be written; the store then keeps what
    /// it held.
    pub fn clear(&self) -> Result<(), String> {
        let mut slots = self.slots.lock();
        let next = Slots::default();
        self.write(&next)?;
        *slots = next;
        drop(slots);
        Ok(())
    }

    /// Write `slots` to the file whole: to a temporary file beside it that
    /// then takes its name.
    fn write(&self, slots: &Slots) -> Result<(), String> {
        let text = serde_json::to_vec_pretty(&slots.body())
            .map_err(|e| format!("encoding the calibration store failed: {e}"))?;
        let temporary = self.path.with_extension("json.writing");
        std::fs::write(&temporary, text)
            .and_then(|()| std::fs::rename(&temporary, &self.path))
            .map_err(|e| format!("writing {} failed: {e}", self.path.display()))?;
        debug!(path = %self.path.display(), "calibration store written");
        Ok(())
    }
}

/// Set the unreadable file at `path` aside under a name of its own, and
/// start the store empty.
fn set_aside(path: &Path, why: &str) -> Slots {
    let stamp = humantime::format_rfc3339_seconds(SystemTime::now())
        .to_string()
        .replace(['-', ':'], "");
    let aside = path.with_file_name(format!("{FILE}.unreadable-{stamp}"));
    match std::fs::rename(path, &aside) {
        Ok(()) => info!(
            path = %path.display(),
            aside = %aside.display(),
            %why,
            "the calibration store could not be read; set aside, and the store starts empty"
        ),
        Err(error) => warn!(
            path = %path.display(),
            %why,
            %error,
            "the calibration store could not be read nor set aside; the store starts empty"
        ),
    }
    Slots::default()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn record(side: PierSide, declination_deg: f64) -> Record {
        Record {
            calibrated_at: humantime::parse_rfc3339("2026-11-12T03:14:15Z").unwrap(),
            calibration: Calibration {
                setup: Setup {
                    pier_side: side,
                    camera: CameraGeometry {
                        pixel_size_um: 1.45,
                        width: 3_840,
                        height: 2_192,
                        binning: 2,
                    },
                    focal_length_mm: 200.0,
                    guide_rates: Some(GuideRates {
                        ra_deg_per_s: 0.002_089,
                        dec_deg_per_s: 0.002_089,
                    }),
                },
                declination_deg,
                ra: Axis {
                    angle_deg: 87.3,
                    rate_px_per_s: 2.52,
                    steps: 12,
                    step: Duration::from_millis(850),
                },
                dec: Some(DecAxis {
                    axis: Axis {
                        angle_deg: 177.1,
                        rate_px_per_s: 2.49,
                        steps: 12,
                        step: Duration::from_millis(830),
                    },
                    backlash: Duration::from_millis(310),
                }),
                warnings: vec![Warning {
                    check: Check::DecRate,
                    value: 0.21,
                    threshold: 0.2,
                }],
            },
        }
    }

    #[test]
    fn a_record_reads_back_as_it_was_written() {
        let record = record(PierSide::East, 12.4);
        let text = serde_json::to_string(&RecordBody::from(&record)).unwrap();
        let back: RecordBody = serde_json::from_str(&text).unwrap();
        assert_eq!(Record::try_from(back).unwrap(), record);
    }

    #[test]
    fn a_record_on_the_wire_has_the_fields_of_the_design() {
        let body = serde_json::to_value(RecordBody::from(&record(PierSide::West, 12.4))).unwrap();
        assert_eq!(body["calibrated_at"], "2026-11-12T03:14:15Z");
        assert_eq!(body["pier_side"], "west");
        assert_eq!(body["ra"]["step"], "850ms");
        assert_eq!(body["dec"]["backlash"], "310ms");
        assert_eq!(
            body["warnings"],
            serde_json::json!([{ "check": "dec_rate", "value": 0.21, "threshold": 0.2 }])
        );
        let orthogonality = body["orthogonality_error_deg"].as_f64().unwrap();
        assert!((orthogonality - 0.2).abs() < 1e-9, "{orthogonality}");
        assert!(body["expected"]["ra_rate_px_per_s"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn a_record_without_a_dec_axis_or_guide_rates_has_none_on_the_wire() {
        let mut record = record(PierSide::East, 0.0);
        record.calibration.dec = None;
        record.calibration.setup.guide_rates = None;
        let body = serde_json::to_value(RecordBody::from(&record)).unwrap();
        for field in ["dec", "orthogonality_error_deg", "guide_rates", "expected"] {
            assert_eq!(body[field], serde_json::Value::Null, "{field}");
        }
    }

    #[test]
    fn every_leg_and_check_has_its_api_name() {
        assert_eq!(
            [
                Leg::West,
                Leg::East,
                Leg::ClearBacklash,
                Leg::North,
                Leg::South
            ]
            .map(leg_name),
            ["west", "east", "clear_backlash", "north", "south"]
        );
        for check in [
            Check::Orthogonality,
            Check::RaRate,
            Check::DecRate,
            Check::RateRatio,
        ] {
            assert_eq!(check_named(check_name(check)), Some(check));
        }
    }

    #[test]
    fn a_stored_record_is_in_the_file_and_in_a_store_opened_again() {
        let dir = tempfile::tempdir().unwrap();
        let store = CalibrationStore::open(dir.path());
        store.put(record(PierSide::West, 12.4)).unwrap();
        let body = store.body();
        assert!(body.west.is_some());
        assert!(body.east.is_none());
        assert_eq!(CalibrationStore::open(dir.path()).body(), body);
        assert!(!dir.path().join("calibration.json.writing").exists());
    }

    #[test]
    fn a_record_made_beyond_60_degrees_is_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let store = CalibrationStore::open(dir.path());
        store.put(record(PierSide::East, 61.0)).unwrap();
        assert_eq!(store.body(), StoreBody::default());
        assert!(!dir.path().join(FILE).exists());
    }

    #[test]
    fn a_record_replaces_only_the_slot_of_its_pier_side() {
        let dir = tempfile::tempdir().unwrap();
        let store = CalibrationStore::open(dir.path());
        store.put(record(PierSide::East, 10.0)).unwrap();
        store.put(record(PierSide::West, 20.0)).unwrap();
        store.put(record(PierSide::East, 30.0)).unwrap();
        let body = store.body();
        assert_eq!(body.east.unwrap().declination_deg, 30.0);
        assert_eq!(body.west.unwrap().declination_deg, 20.0);
    }

    #[test]
    fn clearing_empties_every_slot_in_the_file_too() {
        let dir = tempfile::tempdir().unwrap();
        let store = CalibrationStore::open(dir.path());
        store.put(record(PierSide::East, 10.0)).unwrap();
        store.clear().unwrap();
        assert_eq!(store.body(), StoreBody::default());
        assert_eq!(
            CalibrationStore::open(dir.path()).body(),
            StoreBody::default()
        );
    }

    #[test]
    fn a_file_that_cannot_be_written_leaves_the_slot_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let store = CalibrationStore::open(&dir.path().join("missing"));
        let error = store.put(record(PierSide::East, 10.0)).unwrap_err();
        assert!(error.contains("writing"), "{error}");
        assert_eq!(store.body(), StoreBody::default());
    }

    #[test]
    fn an_unreadable_file_is_set_aside_and_the_store_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), "not a calibration").unwrap();
        let store = CalibrationStore::open(dir.path());
        assert_eq!(store.body(), StoreBody::default());
        assert!(!dir.path().join(FILE).exists());
        let aside: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert!(
            aside[0].starts_with("calibration.json.unreadable-"),
            "{aside:?}"
        );
        assert!(aside[0].ends_with('Z'), "{aside:?}");
    }

    #[test]
    fn a_file_with_a_record_that_does_not_parse_is_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let mut body = serde_json::to_value(StoreBody {
            east: Some(RecordBody::from(&record(PierSide::East, 10.0))),
            ..StoreBody::default()
        })
        .unwrap();
        body["east"]["ra"]["step"] = serde_json::json!("soon");
        std::fs::write(dir.path().join(FILE), body.to_string()).unwrap();
        assert_eq!(
            CalibrationStore::open(dir.path()).body(),
            StoreBody::default()
        );
    }
}

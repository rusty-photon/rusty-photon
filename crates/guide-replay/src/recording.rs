//! A recording: the directory `record.ps1` writes. See
//! `docs/crates/guide-replay.md` (The recording).

use std::fs;
use std::path::{Path, PathBuf};

use ndarray::Array2;
use rp_guiding::SensorType;
use serde::Deserialize;

use crate::imagebytes::{self, DecodeError};

/// Alpaca's `SensorType` for a monochrome sensor.
const ALPACA_MONOCHROME: i64 = 0;

/// Alpaca's `SensorType` for an RGGB mosaic.
const ALPACA_RGGB: i64 = 2;

/// Why a recording cannot be read.
#[derive(Debug, thiserror::Error)]
pub enum RecordingError {
    /// A file cannot be read.
    #[error("cannot read {path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// `camera.json` is no JSON object.
    #[error("{path} is no JSON object: {source}")]
    Json {
        /// The file.
        path: PathBuf,
        /// Why.
        source: serde_json::Error,
    },
    /// `camera.json` names no sensor type the guider reads.
    #[error("sensor type {0:?} is neither monochrome (0) nor RGGB (2)")]
    Sensor(Option<i64>),
    /// An RGGB camera was recorded binned: its frames are no mosaic the
    /// guider can sum.
    #[error("frame {frame} of an RGGB camera is binned {bin} × {bin}; the guider reads an RGGB frame unbinned")]
    BinnedMosaic {
        /// The first binned frame.
        frame: u32,
        /// Its binning.
        bin: u32,
    },
    /// `frames.csv` has a row that cannot be read.
    #[error("{path}: {source}")]
    Csv {
        /// The file.
        path: PathBuf,
        /// Why.
        source: csv::Error,
    },
    /// A frame's file is no frame.
    #[error("{path}: {source}")]
    Decode {
        /// The file.
        path: PathBuf,
        /// Why.
        source: DecodeError,
    },
}

/// What the recording says of its camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Camera {
    /// How the guider reads its frames.
    pub sensor: SensorType,
    /// Its `MaxADU`, when it gave one.
    pub max_adu: Option<u32>,
}

/// One row of `frames.csv`.
#[derive(Debug, Clone, Deserialize)]
struct Row {
    frame: u32,
    file: String,
    start_utc: String,
    bin: u32,
    error: String,
}

/// A recording's camera and its frames, in the order they were taken.
#[derive(Debug, Clone)]
pub struct Recording {
    dir: PathBuf,
    camera: Camera,
    rows: Vec<Row>,
}

/// One frame of a recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    /// Its number, from 1.
    pub frame: u32,
    /// The time its exposure was asked for, in UTC.
    pub start_utc: String,
    /// Its pixels, indexed `[[y, x]]`, or the recorder's error for a frame
    /// that did not come.
    pub pixels: Result<Array2<i32>, String>,
}

impl Recording {
    /// Opens the recording in `dir`: its `camera.json` and `frames.csv`.
    ///
    /// # Errors
    ///
    /// A [`RecordingError`] when either cannot be read.
    pub fn open(dir: &Path) -> Result<Self, RecordingError> {
        let camera = camera(&dir.join("camera.json"))?;
        let index = dir.join("frames.csv");
        let csv_error = |source| RecordingError::Csv {
            path: index.clone(),
            source,
        };
        let rows = csv::Reader::from_path(&index)
            .map_err(csv_error)?
            .deserialize()
            .collect::<Result<Vec<Row>, _>>()
            .map_err(csv_error)?;
        if camera.sensor == SensorType::Rggb {
            if let Some(row) = rows.iter().find(|row| row.bin != 1) {
                return Err(RecordingError::BinnedMosaic {
                    frame: row.frame,
                    bin: row.bin,
                });
            }
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            camera,
            rows,
        })
    }

    /// What the recording says of its camera.
    #[must_use]
    pub const fn camera(&self) -> Camera {
        self.camera
    }

    /// How many frames `frames.csv` lists, failed ones included.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether `frames.csv` lists no frame.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The frames, read one at a time.
    pub fn frames(&self) -> impl Iterator<Item = Result<Recorded, RecordingError>> + '_ {
        self.rows.iter().map(|row| self.read(row))
    }

    fn read(&self, row: &Row) -> Result<Recorded, RecordingError> {
        let pixels = if row.error.is_empty() {
            let path = self.dir.join(&row.file);
            let body = fs::read(&path).map_err(|source| RecordingError::Io {
                path: path.clone(),
                source,
            })?;
            Ok(imagebytes::decode(&body)
                .map_err(|source| RecordingError::Decode { path, source })?)
        } else {
            Err(row.error.clone())
        };
        Ok(Recorded {
            frame: row.frame,
            start_utc: row.start_utc.clone(),
            pixels,
        })
    }
}

fn camera(path: &Path) -> Result<Camera, RecordingError> {
    let text = fs::read_to_string(path).map_err(|source| RecordingError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let json: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&text).map_err(|source| RecordingError::Json {
            path: path.to_path_buf(),
            source,
        })?;
    let sensor = match json.get("sensortype").and_then(serde_json::Value::as_i64) {
        Some(ALPACA_MONOCHROME) => SensorType::Monochrome,
        Some(ALPACA_RGGB) => SensorType::Rggb,
        other => return Err(RecordingError::Sensor(other)),
    };
    let max_adu = json
        .get("maxadu")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());
    Ok(Camera { sensor, max_adu })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::test_support::{write_camera, write_recording};

    #[test]
    fn a_recording_lists_its_frames_in_order_with_the_failed_ones() {
        let dir = tempfile::tempdir().unwrap();
        let first = Array2::from_elem((2, 3), 100_u16);
        let third = Array2::from_elem((2, 3), 300_u16);
        write_recording(dir.path(), 0, &[Some(first), None, Some(third)]);
        let recording = Recording::open(dir.path()).unwrap();
        assert_eq!(
            recording.camera(),
            Camera {
                sensor: SensorType::Monochrome,
                max_adu: Some(65_535)
            }
        );
        assert_eq!(recording.len(), 3);
        let frames: Vec<Recorded> = recording.frames().map(Result::unwrap).collect();
        assert_eq!(
            frames.iter().map(|frame| frame.frame).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(frames[0].pixels, Ok(Array2::from_elem((2, 3), 100)));
        assert_eq!(frames[1].pixels, Err("imagearray HTTP 500".to_owned()));
        assert_eq!(frames[2].pixels, Ok(Array2::from_elem((2, 3), 300)));
        assert_eq!(frames[2].start_utc, "2026-11-23T03:00:05.000Z");
    }

    #[test]
    fn an_rggb_camera_is_read_as_one_and_maxadu_may_be_missing() {
        let dir = tempfile::tempdir().unwrap();
        write_camera(dir.path(), r#"{"sensortype": 2}"#);
        fs::write(
            dir.path().join("frames.csv"),
            "frame,file,start_utc,bin,error\n",
        )
        .unwrap();
        let recording = Recording::open(dir.path()).unwrap();
        assert_eq!(
            recording.camera(),
            Camera {
                sensor: SensorType::Rggb,
                max_adu: None
            }
        );
        assert!(recording.is_empty());
    }

    #[test]
    fn a_sensor_the_guider_does_not_read_is_refused() {
        for (json, sensor) in [
            (r#"{"sensortype": 1}"#, Some(1)),
            (r#"{"sensortype": "0"}"#, None),
            ("{}", None),
        ] {
            let dir = tempfile::tempdir().unwrap();
            write_camera(dir.path(), json);
            let error = Recording::open(dir.path()).unwrap_err();
            assert!(
                matches!(error, RecordingError::Sensor(found) if found == sensor),
                "{json}: {error}"
            );
        }
    }

    #[test]
    fn a_missing_or_broken_file_is_an_error_that_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let error = Recording::open(dir.path()).unwrap_err();
        assert!(matches!(&error, RecordingError::Io { path, .. } if path.ends_with("camera.json")));

        write_camera(dir.path(), "[1]");
        let error = Recording::open(dir.path()).unwrap_err();
        assert!(matches!(&error, RecordingError::Json { .. }), "{error}");

        write_camera(dir.path(), r#"{"sensortype": 0}"#);
        let error = Recording::open(dir.path()).unwrap_err();
        assert!(matches!(&error, RecordingError::Csv { path, .. } if path.ends_with("frames.csv")));

        fs::write(
            dir.path().join("frames.csv"),
            "frame,file,start_utc,bin,error\nx,,,1,\n",
        )
        .unwrap();
        let error = Recording::open(dir.path()).unwrap_err();
        assert!(matches!(&error, RecordingError::Csv { .. }), "{error}");
    }

    #[test]
    fn a_frame_file_that_is_missing_or_no_frame_is_an_error_that_names_it() {
        let dir = tempfile::tempdir().unwrap();
        write_recording(dir.path(), 0, &[Some(Array2::from_elem((1, 1), 1_u16))]);
        let file = dir.path().join("frame-00001.imagebytes");
        fs::write(&file, [0; 10]).unwrap();
        let recording = Recording::open(dir.path()).unwrap();
        let error = recording.frames().next().unwrap().unwrap_err();
        assert!(matches!(&error, RecordingError::Decode { path, .. } if *path == file));

        fs::remove_file(&file).unwrap();
        let error = recording.frames().next().unwrap().unwrap_err();
        assert!(matches!(&error, RecordingError::Io { path, .. } if *path == file));
    }

    #[test]
    fn a_binned_rggb_recording_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write_camera(dir.path(), r#"{"sensortype": 2}"#);
        fs::write(
            dir.path().join("frames.csv"),
            "frame,file,start_utc,bin,error\n1,,2026-11-23T03:00:00.000Z,1,\n2,,,2,\n",
        )
        .unwrap();
        let error = Recording::open(dir.path()).unwrap_err();
        assert!(
            matches!(error, RecordingError::BinnedMosaic { frame: 2, bin: 2 }),
            "{error}"
        );
    }
}

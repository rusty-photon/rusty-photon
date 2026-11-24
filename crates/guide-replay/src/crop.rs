//! Crops: a sample of a recording's frames, cut to one region of the
//! sensor and written as FITS, as a smaller recording a test can replay.
//! See `docs/crates/guide-replay.md` (Crops).

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use ndarray::s;
use rp_fits::writer::{write_u16_image, Keyword, KeywordValue};
use rp_fits::FitsError;

use crate::recording::{Recording, RecordingError};

/// A region of the sensor, in pixels of the recorded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    /// Its left column.
    pub x: usize,
    /// Its top row.
    pub y: usize,
    /// Its width.
    pub width: usize,
    /// Its height.
    pub height: usize,
}

/// Why a recording cannot be cropped.
#[derive(Debug, thiserror::Error)]
pub enum CropError {
    /// The recording cannot be read.
    #[error(transparent)]
    Recording(#[from] RecordingError),
    /// The region is empty, or reaches past a frame.
    #[error("frame {frame} is {width} × {height}; the region does not lie inside it")]
    Outside {
        /// The frame.
        frame: u32,
        /// Its width.
        width: usize,
        /// Its height.
        height: usize,
    },
    /// The directory the crop goes to already holds files.
    #[error("{path} already holds files; a crop goes to a new or empty directory")]
    Occupied {
        /// The directory.
        path: PathBuf,
    },
    /// No frame the crop would take came from the recorder.
    #[error("the recording has no frame for the crop to take")]
    Empty,
    /// Every frame but none is asked for.
    #[error("a crop takes every frame or every Nth, and N is 0")]
    NoFrames,
    /// A file cannot be written.
    #[error("cannot write {path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// A crop cannot be written as FITS.
    #[error("cannot write {path}: {source}")]
    Fits {
        /// The file.
        path: PathBuf,
        /// Why.
        source: FitsError,
    },
}

/// Writes every `every`th frame of the recording in `from`, the first
/// among them, cut to `region`, into `to` as a recording of FITS frames.
///
/// The crop has its own `frames.csv`, `camera.json` copied, and
/// `frame-NNNNN.fits` under each frame's number. A frame the recorder
/// did not get is left out. `to` must be new or empty, so that a crop
/// never writes over a recording or an earlier crop, and a refused
/// region writes nothing. Returns how many frames were written.
///
/// # Errors
///
/// A [`CropError`] when the recording cannot be read, the region does
/// not lie inside a frame, or a file cannot be written.
pub fn crop(from: &Path, to: &Path, region: Region, every: u32) -> Result<usize, CropError> {
    if every == 0 {
        return Err(CropError::NoFrames);
    }
    // Only a directory that is not there counts as new: one that cannot
    // be listed may hold a recording.
    match fs::read_dir(to) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(CropError::Occupied {
                    path: to.to_path_buf(),
                });
            }
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(CropError::Io {
                path: to.to_path_buf(),
                source,
            })
        }
    }
    let recording = Recording::open(from)?;
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| CropError::Io { path, source }
    };
    // Nothing is written until the first frame to take fits the region,
    // so a refused region leaves `to` as it was.
    let mut started = false;
    let mut index = vec!["frame,file,start_utc,bin,numx,numy,error".to_owned()];
    for recorded in recording.frames() {
        let recorded = recorded?;
        let Ok(pixels) = recorded.pixels else {
            continue;
        };
        if recorded.frame.saturating_sub(1).checked_rem(every) != Some(0) {
            continue;
        }
        let (rows, columns) = pixels.dim();
        let end_x = region
            .x
            .checked_add(region.width)
            .filter(|end| region.width > 0 && *end <= columns);
        let end_y = region
            .y
            .checked_add(region.height)
            .filter(|end| region.height > 0 && *end <= rows);
        let (Some(end_x), Some(end_y)) = (end_x, end_y) else {
            return Err(CropError::Outside {
                frame: recorded.frame,
                width: columns,
                height: rows,
            });
        };
        if !started {
            fs::create_dir_all(to).map_err(io(to))?;
            let camera = to.join("camera.json");
            fs::copy(from.join("camera.json"), &camera).map_err(io(&camera))?;
            started = true;
        }
        let cut: Vec<u16> = pixels
            .slice(s![region.y..end_y, region.x..end_x])
            .iter()
            .map(|value| u16::try_from((*value).max(0)).unwrap_or(u16::MAX))
            .collect();
        let file = format!("frame-{:05}.fits", recorded.frame);
        let path = to.join(&file);
        let fits = |source| CropError::Fits {
            path: path.clone(),
            source,
        };
        // FITS writes a UTC time without a zone suffix.
        let date = Keyword::new(
            "DATE-OBS",
            KeywordValue::Str(recorded.start_utc.trim_end_matches('Z').to_owned()),
        )
        .map_err(fits)?;
        let row_order =
            Keyword::new("ROWORDER", KeywordValue::Str("TOP-DOWN".to_owned())).map_err(fits)?;
        let mut out = BufWriter::new(File::create(&path).map_err(io(&path))?);
        write_u16_image(
            &mut out,
            &cut,
            region.width,
            region.height,
            &[date, row_order],
        )
        .map_err(fits)?;
        out.flush().map_err(io(&path))?;
        index.push(format!(
            "{},{file},{},{},{},{},",
            recorded.frame, recorded.start_utc, recorded.bin, region.width, region.height
        ));
    }
    if !started {
        return Err(CropError::Empty);
    }
    let written = index.len().saturating_sub(1);
    index.push(String::new());
    let csv = to.join("frames.csv");
    fs::write(&csv, index.join("\n")).map_err(io(&csv))?;
    Ok(written)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use ndarray::Array2;

    use super::*;
    use crate::test_support::write_recording;

    fn frame(base: u16) -> Array2<u16> {
        Array2::from_shape_fn((6, 8), |(y, x)| base + (10 * y + x) as u16)
    }

    #[test]
    fn every_nth_frame_is_cut_to_the_region_and_reads_back_as_a_recording() {
        let from = tempfile::tempdir().unwrap();
        let to = tempfile::tempdir().unwrap();
        let frames: Vec<Option<Array2<u16>>> =
            (0..5).map(|n| Some(frame(1_000 * (n + 1)))).collect();
        write_recording(from.path(), 0, &frames);
        let region = Region {
            x: 2,
            y: 1,
            width: 4,
            height: 3,
        };
        assert_eq!(crop(from.path(), to.path(), region, 2).unwrap(), 3);
        let cropped = Recording::open(to.path()).unwrap();
        assert_eq!(
            cropped.camera(),
            Recording::open(from.path()).unwrap().camera()
        );
        let read: Vec<_> = cropped.frames().map(Result::unwrap).collect();
        assert_eq!(
            read.iter().map(|frame| frame.frame).collect::<Vec<_>>(),
            [1, 3, 5]
        );
        let expected = frame(3_000).slice(s![1..4, 2..6]).mapv(i32::from);
        assert_eq!(read[1].pixels, Ok(expected));
        assert_eq!(read[1].start_utc, "2026-11-23T03:00:05.000Z");
        assert_eq!(read[1].bin, 2);
        assert!(to.path().join("frame-00003.fits").exists());
    }

    #[test]
    fn a_frame_the_recorder_did_not_get_is_left_out() {
        let from = tempfile::tempdir().unwrap();
        let to = tempfile::tempdir().unwrap();
        write_recording(from.path(), 0, &[Some(frame(1)), None, Some(frame(3))]);
        let region = Region {
            x: 0,
            y: 0,
            width: 8,
            height: 6,
        };
        assert_eq!(crop(from.path(), to.path(), region, 1).unwrap(), 2);
        let frames: Vec<u32> = Recording::open(to.path())
            .unwrap()
            .frames()
            .map(|frame| frame.unwrap().frame)
            .collect();
        assert_eq!(frames, [1, 3]);
    }

    #[test]
    fn a_region_outside_the_frame_or_no_frames_is_refused() {
        let from = tempfile::tempdir().unwrap();
        write_recording(from.path(), 0, &[Some(frame(1))]);
        for region in [
            Region {
                x: 5,
                y: 0,
                width: 4,
                height: 2,
            },
            Region {
                x: 0,
                y: 4,
                width: 2,
                height: 3,
            },
            Region {
                x: 0,
                y: 0,
                width: 0,
                height: 2,
            },
        ] {
            let to = tempfile::tempdir().unwrap();
            let error = crop(from.path(), to.path(), region, 1).unwrap_err();
            assert!(
                matches!(
                    error,
                    CropError::Outside {
                        frame: 1,
                        width: 8,
                        height: 6
                    }
                ),
                "{region:?}: {error}"
            );
            assert!(
                fs::read_dir(to.path()).unwrap().next().is_none(),
                "{region:?} left files behind"
            );
        }
        let to = tempfile::tempdir().unwrap();
        let region = Region {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        assert!(matches!(
            crop(from.path(), to.path(), region, 0),
            Err(CropError::NoFrames)
        ));
    }

    #[test]
    fn a_recording_that_cannot_be_read_or_a_crop_that_cannot_be_written_is_an_error() {
        let to = tempfile::tempdir().unwrap();
        let region = Region {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        let missing = to.path().join("missing");
        assert!(matches!(
            crop(&missing, to.path(), region, 1),
            Err(CropError::Recording(_))
        ));
        let from = tempfile::tempdir().unwrap();
        write_recording(from.path(), 0, &[Some(frame(1))]);
        let blocked = to.path().join("blocked");
        fs::write(&blocked, "a file where the crop's directory goes").unwrap();
        assert!(matches!(
            crop(from.path(), &blocked, region, 1),
            Err(CropError::Io { .. })
        ));
    }

    #[test]
    fn a_crop_never_writes_into_a_directory_that_holds_files() {
        let from = tempfile::tempdir().unwrap();
        write_recording(from.path(), 0, &[Some(frame(1))]);
        let camera = fs::read(from.path().join("camera.json")).unwrap();
        let region = Region {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let error = crop(from.path(), from.path(), region, 1).unwrap_err();
        assert!(
            matches!(&error, CropError::Occupied { path } if path == from.path()),
            "{error}"
        );
        assert_eq!(fs::read(from.path().join("camera.json")).unwrap(), camera);
        assert_eq!(Recording::open(from.path()).unwrap().len(), 1);
    }

    #[test]
    fn a_crop_s_frames_carry_their_time_without_a_zone_and_their_row_order() {
        let from = tempfile::tempdir().unwrap();
        let to = tempfile::tempdir().unwrap();
        write_recording(from.path(), 0, &[Some(frame(1))]);
        let region = Region {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        crop(from.path(), to.path(), region, 1).unwrap();
        let header = fs::read(to.path().join("frame-00001.fits")).unwrap();
        let header = String::from_utf8_lossy(&header[..2880]);
        assert!(
            header.contains("DATE-OBS= '2026-11-23T03:00:00.000'"),
            "{header}"
        );
        assert!(header.contains("ROWORDER= 'TOP-DOWN'"), "{header}");
    }

    #[test]
    fn a_recording_with_no_frame_to_take_is_refused_and_writes_nothing() {
        let from = tempfile::tempdir().unwrap();
        let to = tempfile::tempdir().unwrap();
        write_recording(from.path(), 0, &[None, Some(frame(2))]);
        let region = Region {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        assert!(matches!(
            crop(from.path(), to.path(), region, 2),
            Err(CropError::Empty)
        ));
        assert!(fs::read_dir(to.path()).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_that_cannot_be_listed_is_never_written_into() {
        use std::os::unix::fs::PermissionsExt;
        let from = tempfile::tempdir().unwrap();
        write_recording(from.path(), 0, &[Some(frame(1))]);
        let to = tempfile::tempdir().unwrap();
        let held = to.path().join("camera.json");
        fs::write(&held, "a recording's camera").unwrap();
        fs::set_permissions(to.path(), fs::Permissions::from_mode(0o300)).unwrap();
        let region = Region {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let result = crop(from.path(), to.path(), region, 1);
        fs::set_permissions(to.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err(), "the crop went ahead");
        assert_eq!(fs::read_to_string(&held).unwrap(), "a recording's camera");
    }
}

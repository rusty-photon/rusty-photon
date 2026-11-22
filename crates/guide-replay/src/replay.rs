//! The replay: a recording's frames through the guider's star selection
//! and measurement. See `docs/crates/guide-replay.md` (The replay).

use std::io::Write;

use rp_guiding::{
    Frame, FrameError, FrameMeasurement, GuideStars, Offset, StarParams, StarReading,
};
use serde::Serialize;

use crate::recording::{Recording, RecordingError};
use crate::report::{Report, StarReport, Tolerance};

/// Why a replay stopped.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    /// The recording cannot be read.
    #[error(transparent)]
    Recording(#[from] RecordingError),
    /// A frame leaves the guider nothing to measure.
    #[error("frame {frame}: {source}")]
    Frame {
        /// The frame.
        frame: u32,
        /// Why.
        source: FrameError,
    },
}

/// What became of a guide star in a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reading {
    /// Usable.
    Usable,
    /// Not found.
    NotFound,
    /// Found below the lowest signal-to-noise ratio.
    Faint,
    /// Found with its flux too far from its reference.
    FluxChanged,
}

/// One guide star in one measured frame: a row of `replay.csv`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StarRow {
    /// The frame.
    pub frame: u32,
    /// When its exposure was asked for, in UTC.
    pub start_utc: String,
    /// The star, from 0.
    pub star: usize,
    /// What became of it.
    pub reading: Reading,
    /// Its position, when usable.
    pub x: Option<f64>,
    /// Its position, when usable.
    pub y: Option<f64>,
    /// Its offset from where it was selected, when usable.
    pub offset_x: Option<f64>,
    /// Its offset from where it was selected, when usable.
    pub offset_y: Option<f64>,
    /// Its offset less the frame's, when usable in a frame that is not
    /// star-lost.
    pub deviation_x: Option<f64>,
    /// Its offset less the frame's, when usable in a frame that is not
    /// star-lost.
    pub deviation_y: Option<f64>,
    /// Its signal-to-noise ratio, when usable or faint.
    pub snr: Option<f64>,
    /// Its half-flux diameter, when usable.
    pub hfd: Option<f64>,
    /// Its flux, when usable or when its flux changed.
    pub flux: Option<f64>,
    /// The frame's offset, when it is not star-lost.
    pub frame_offset_x: Option<f64>,
    /// The frame's offset, when it is not star-lost.
    pub frame_offset_y: Option<f64>,
    /// How many guide stars are usable in the frame.
    pub usable: usize,
    /// Whether the frame is star-lost.
    pub star_lost: bool,
    /// For a star-lost frame, whether a fresh selection finds no stars in
    /// it either.
    pub clouded: Option<bool>,
}

/// A replay's rows and its report.
#[derive(Debug, Clone, PartialEq)]
pub struct Replay {
    /// One row for each guide star in each measured frame.
    pub rows: Vec<StarRow>,
    /// What it found, held to its tolerance.
    pub report: Report,
}

/// What a guide star's frames add up to.
#[derive(Debug, Clone, Default)]
struct Tally {
    selected_at: Option<[f64; 2]>,
    not_found: usize,
    faint: usize,
    flux_changed: usize,
    /// Its deviation in each frame it is usable in that is not
    /// star-lost.
    deviations: Vec<[f64; 2]>,
}

impl Tally {
    /// Its report, and how many of its deviations are outliers.
    fn report(&self, star: usize, clear: usize, outlier_px: f64) -> (StarReport, usize) {
        let spread = spread(&self.deviations);
        let (mean_deviation_px, scatter_px, largest_deviation_px, outliers) =
            spread.map_or((None, None, None, 0), |(mean, scatter)| {
                let [x_mean, y_mean] = mean;
                let distances = self
                    .deviations
                    .iter()
                    .map(|[x, y]| (x - x_mean).hypot(y - y_mean));
                let largest = distances.clone().fold(0.0, f64::max);
                let outliers = distances.filter(|distance| *distance >= outlier_px).count();
                (Some(mean), Some(scatter), Some(largest), outliers)
            });
        let report = StarReport {
            star,
            selected_at: self.selected_at,
            clear,
            usable: self.deviations.len(),
            not_found: self.not_found,
            faint: self.faint,
            flux_changed: self.flux_changed,
            mean_deviation_px,
            scatter_px,
            largest_deviation_px,
        };
        (report, outliers)
    }
}

/// The mean of `deviations` on each axis, and their standard deviation
/// about it; `None` without a deviation.
fn spread(deviations: &[[f64; 2]]) -> Option<([f64; 2], [f64; 2])> {
    let count = f64::from(
        u32::try_from(deviations.len())
            .ok()
            .filter(|count| *count > 0)?,
    );
    let [x_sum, y_sum] = deviations
        .iter()
        .fold([0.0, 0.0], |[x_sum, y_sum], [x, y]| [x_sum + x, y_sum + y]);
    let (x_mean, y_mean) = (x_sum / count, y_sum / count);
    let [x_squares, y_squares] =
        deviations
            .iter()
            .fold([0.0, 0.0], |[x_squares, y_squares], [x, y]| {
                let (dx, dy) = (x - x_mean, y - y_mean);
                [dx.mul_add(dx, x_squares), dy.mul_add(dy, y_squares)]
            });
    Some((
        [x_mean, y_mean],
        [(x_squares / count).sqrt(), (y_squares / count).sqrt()],
    ))
}

/// Replays `recording`: selects guide stars with `params` on the first
/// frame that has enough of them, measures every frame after it, and
/// holds what it finds to `tolerance`.
///
/// # Errors
///
/// A [`ReplayError`] when a frame cannot be read or measured.
pub fn replay(
    recording: &Recording,
    params: StarParams,
    tolerance: Tolerance,
) -> Result<Replay, ReplayError> {
    let camera = recording.camera();
    let mut report = Report {
        frames: recording.len(),
        failed: 0,
        before_selection: 0,
        measured: 0,
        clear: 0,
        clouded: 0,
        lost: 0,
        stars: Vec::new(),
        deviations: 0,
        outliers: 0,
        tolerance,
        failures: Vec::new(),
    };
    let mut stars: Option<GuideStars> = None;
    let mut tallies: Vec<Tally> = Vec::new();
    let mut rows = Vec::new();
    for recorded in recording.frames() {
        let recorded = recorded?;
        let Ok(pixels) = recorded.pixels else {
            report.failed = report.failed.saturating_add(1);
            continue;
        };
        let frame =
            Frame::new(&pixels.view(), camera.sensor, camera.max_adu).map_err(|source| {
                ReplayError::Frame {
                    frame: recorded.frame,
                    source,
                }
            })?;
        let measurement = if let Some(stars) = stars.as_mut() {
            stars.measure(&frame)
        } else if let Ok((selected, measurement)) = GuideStars::select(&frame, params) {
            stars = Some(selected);
            tallies = measurement
                .stars
                .iter()
                .map(|reading| Tally {
                    selected_at: usable(reading).map(|star| [star.position.x, star.position.y]),
                    ..Tally::default()
                })
                .collect();
            measurement
        } else {
            report.before_selection = report.before_selection.saturating_add(1);
            continue;
        };
        report.measured = report.measured.saturating_add(1);
        let clouded = measurement
            .star_lost
            .then(|| dimmed(&measurement) || GuideStars::select(&frame, params).is_err());
        match clouded {
            Some(true) => report.clouded = report.clouded.saturating_add(1),
            Some(false) => report.lost = report.lost.saturating_add(1),
            None => report.clear = report.clear.saturating_add(1),
        }
        tally_frame(&measurement, &mut tallies);
        rows.extend(rows_of(
            recorded.frame,
            &recorded.start_utc,
            &measurement,
            clouded,
        ));
    }
    for (star, tally) in tallies.iter().enumerate() {
        let (star_report, outliers) = tally.report(star, report.clear, tolerance.outlier_px);
        report.deviations = report.deviations.saturating_add(star_report.usable);
        report.outliers = report.outliers.saturating_add(outliers);
        report.stars.push(star_report);
    }
    report.judge();
    Ok(Replay { rows, report })
}

/// Whether a star-lost frame's stars were seen and dimmed: each guide
/// star that is not usable is faint, or found with less flux than its
/// reference.
fn dimmed(measurement: &FrameMeasurement) -> bool {
    measurement.stars.iter().all(|reading| match reading {
        StarReading::Usable(_) | StarReading::Faint { .. } => true,
        StarReading::FluxChanged { flux, reference } => flux < reference,
        StarReading::NotFound => false,
    })
}

/// Adds a frame that is not star-lost to its stars' tallies.
fn tally_frame(measurement: &FrameMeasurement, tallies: &mut [Tally]) {
    let Some(frame_offset) = measurement.offset else {
        return;
    };
    for (reading, tally) in measurement.stars.iter().zip(tallies.iter_mut()) {
        match reading {
            StarReading::Usable(star) => {
                tally.deviations.push(deviation(star.offset, frame_offset));
            }
            StarReading::NotFound => tally.not_found = tally.not_found.saturating_add(1),
            StarReading::Faint { .. } => tally.faint = tally.faint.saturating_add(1),
            StarReading::FluxChanged { .. } => {
                tally.flux_changed = tally.flux_changed.saturating_add(1);
            }
        }
    }
}

fn rows_of<'a>(
    frame: u32,
    start_utc: &'a str,
    measurement: &'a FrameMeasurement,
    clouded: Option<bool>,
) -> impl Iterator<Item = StarRow> + 'a {
    let frame_offset = measurement.offset;
    measurement
        .stars
        .iter()
        .enumerate()
        .map(move |(star, reading)| {
            let found = usable(reading);
            let deviation = found
                .zip(frame_offset)
                .map(|(found, frame_offset)| deviation(found.offset, frame_offset));
            StarRow {
                frame,
                start_utc: start_utc.to_owned(),
                star,
                reading: match reading {
                    StarReading::Usable(_) => Reading::Usable,
                    StarReading::NotFound => Reading::NotFound,
                    StarReading::Faint { .. } => Reading::Faint,
                    StarReading::FluxChanged { .. } => Reading::FluxChanged,
                },
                x: found.map(|found| found.position.x),
                y: found.map(|found| found.position.y),
                offset_x: found.map(|found| found.offset.x),
                offset_y: found.map(|found| found.offset.y),
                deviation_x: deviation.map(|[x, _]| x),
                deviation_y: deviation.map(|[_, y]| y),
                snr: match reading {
                    StarReading::Usable(found) => Some(found.snr),
                    StarReading::Faint { snr } => Some(*snr),
                    StarReading::NotFound | StarReading::FluxChanged { .. } => None,
                },
                hfd: found.map(|found| found.hfd),
                flux: match reading {
                    StarReading::Usable(found) => Some(found.flux),
                    StarReading::FluxChanged { flux, .. } => Some(*flux),
                    StarReading::NotFound | StarReading::Faint { .. } => None,
                },
                frame_offset_x: frame_offset.map(|offset| offset.x),
                frame_offset_y: frame_offset.map(|offset| offset.y),
                usable: measurement.usable,
                star_lost: measurement.star_lost,
                clouded,
            }
        })
}

const fn usable(reading: &StarReading) -> Option<&rp_guiding::Usable> {
    match reading {
        StarReading::Usable(star) => Some(star),
        StarReading::NotFound | StarReading::Faint { .. } | StarReading::FluxChanged { .. } => None,
    }
}

fn deviation(star: Offset, frame: Offset) -> [f64; 2] {
    [star.x - frame.x, star.y - frame.y]
}

/// Writes `rows` as `replay.csv`, under a header row when there are any.
///
/// # Errors
///
/// When `out` cannot be written.
pub fn write_csv(rows: &[StarRow], out: impl Write) -> csv::Result<()> {
    let mut writer = csv::Writer::from_writer(out);
    for row in rows {
        writer.serialize(row)?;
    }
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::time::Duration;

    use guide_simulator::{Cloud, FrameTruth, PierSide, SkyOffset};
    use ndarray::{s, Array2};

    use super::*;
    use crate::report::Failure;
    use crate::test_support::{record, sky, write_recording};

    /// The tolerance with as few clear frames as a short replay has.
    fn tolerance(min_clear_frames: usize) -> Tolerance {
        Tolerance {
            min_clear_frames,
            ..Tolerance::default()
        }
    }

    fn replay_of(dir: &std::path::Path, min_clear_frames: usize) -> Replay {
        let recording = Recording::open(dir).unwrap();
        replay(
            &recording,
            StarParams::default(),
            tolerance(min_clear_frames),
        )
        .unwrap()
    }

    #[test]
    fn a_tracked_field_in_seeing_keeps_its_stars_and_they_agree() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), &mut sky(|_| {}), 120, |_, _, _| {});
        let report = replay_of(dir.path(), 120).report;
        assert_eq!(report.failures, []);
        assert_eq!(
            (report.measured, report.clear, report.clouded, report.lost),
            (120, 120, 0, 0)
        );
        assert_eq!(report.stars.len(), 8);
        for star in &report.stars {
            assert_eq!(star.usable, 120, "star {}", star.star);
            let [x, y] = star.scatter_px.unwrap();
            assert!(x < 0.3 && y < 0.3, "star {}: {x} {y}", star.star);
        }
        assert_eq!(report.deviations, 960);
        assert_eq!(report.outliers, 0);
    }

    #[test]
    fn the_deviation_is_each_star_s_offset_less_its_frame_s() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), &mut sky(|_| {}), 5, |_, _, _| {});
        let replay = replay_of(dir.path(), 5);
        assert_eq!(replay.rows.len(), 40);
        for row in &replay.rows {
            assert_eq!(row.reading, Reading::Usable);
            assert_eq!(
                row.deviation_x.unwrap(),
                row.offset_x.unwrap() - row.frame_offset_x.unwrap()
            );
            assert_eq!(
                row.deviation_y.unwrap(),
                row.offset_y.unwrap() - row.frame_offset_y.unwrap()
            );
            assert_eq!((row.usable, row.star_lost, row.clouded), (8, false, None));
        }
        let first = &replay.rows[0];
        assert_eq!((first.frame, first.star), (1, 0));
        assert_eq!((first.offset_x, first.offset_y), (Some(0.0), Some(0.0)));
    }

    #[test]
    fn frames_before_any_star_and_failed_frames_are_counted_and_not_measured() {
        let dir = tempfile::tempdir().unwrap();
        let mut sim = sky(|config| {
            config.sky.clouds = vec![Cloud {
                start: Duration::ZERO,
                duration: Duration::from_secs(10),
                ramp: Duration::ZERO,
                transmission: 0.0,
            }];
        });
        let mut shots: Vec<Option<Array2<u16>>> = (0..8)
            .map(|_| {
                let shot = sim.expose(Duration::from_secs(2));
                sim.advance(Duration::from_millis(500));
                Some(shot.pixels)
            })
            .collect();
        shots.insert(6, None);
        write_recording(dir.path(), 0, &shots);
        let replay = replay_of(dir.path(), 1);
        let report = &replay.report;
        assert_eq!(
            (
                report.frames,
                report.failed,
                report.before_selection,
                report.measured
            ),
            (9, 1, 4, 4)
        );
        assert_eq!(replay.rows.first().unwrap().frame, 5);
    }

    #[test]
    fn a_cloud_s_star_lost_frames_are_clouded_and_fail_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut sim = sky(|config| {
            config.sky.clouds = vec![Cloud {
                start: Duration::from_secs(20),
                duration: Duration::from_secs(15),
                ramp: Duration::from_secs(1),
                transmission: 0.0,
            }];
        });
        record(dir.path(), &mut sim, 30, |_, _, _| {});
        let replay = replay_of(dir.path(), 20);
        let report = &replay.report;
        assert!(report.clouded >= 3, "{report:?}");
        assert_eq!(report.lost, 0);
        assert_eq!(report.clear + report.clouded, 30);
        assert_eq!(report.failures, []);
        let lost_row = replay.rows.iter().find(|row| row.star_lost).unwrap();
        assert_eq!(lost_row.clouded, Some(true));
        assert_eq!(lost_row.frame_offset_x, None);
        assert_eq!(lost_row.deviation_x, None);
    }

    #[test]
    fn stars_the_measurement_loses_while_they_are_there_fail_the_replay() {
        let dir = tempfile::tempdir().unwrap();
        let mut sim = sky(|_| {});
        let shots: Vec<Option<Array2<u16>>> = (1..=20)
            .map(|n| {
                if n == 11 {
                    sim.mount_mut()
                        .slew(SkyOffset::new(60.0, 40.0), Duration::ZERO, PierSide::East)
                        .unwrap();
                }
                let shot = sim.expose(Duration::from_secs(2));
                sim.advance(Duration::from_millis(500));
                Some(shot.pixels)
            })
            .collect();
        write_recording(dir.path(), 0, &shots);
        let replay = replay_of(dir.path(), 10);
        let report = &replay.report;
        assert_eq!((report.clear, report.lost, report.clouded), (10, 10, 0));
        assert!(report.failures.contains(&Failure::Lost { frames: 10 }));
        let lost_row = replay.rows.iter().find(|row| row.star_lost).unwrap();
        assert_eq!(
            (lost_row.reading, lost_row.clouded),
            (Reading::NotFound, Some(false))
        );
        for star in &report.stars {
            assert_eq!((star.usable, star.not_found), (10, 0), "star {}", star.star);
        }
    }

    /// Moves the brightest star's light `by` px along x, within the
    /// frame's square around it.
    fn move_brightest(pixels: &mut Array2<u16>, truth: &FrameTruth, by: usize) {
        let star = &truth.stars[0];
        let (x, y) = (
            star.position.x.round() as usize,
            star.position.y.round() as usize,
        );
        let window = pixels.slice(s![y - 8..y + 8, x - 8..x + 8]).to_owned();
        let floor = *window.iter().min().unwrap();
        pixels.slice_mut(s![y - 8..y + 8, x - 8..x + 8]).fill(floor);
        pixels
            .slice_mut(s![y - 8..y + 8, x - 8 + by..x + 8 + by])
            .assign(&window);
    }

    #[test]
    fn a_star_measured_off_its_light_for_half_the_recording_scatters() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), &mut sky(|_| {}), 40, |n, pixels, truth| {
            if n > 20 {
                move_brightest(pixels, truth, 3);
            }
        });
        let report = replay_of(dir.path(), 40).report;
        let scattered: Vec<usize> = report
            .failures
            .iter()
            .filter_map(|failure| match failure {
                Failure::Scattered { star, .. } => Some(*star),
                _ => None,
            })
            .collect();
        assert_eq!(scattered, [0]);
        let [x, _] = report.stars[0].scatter_px.unwrap();
        assert!((1.3..1.7).contains(&x), "{x}");
    }

    #[test]
    fn a_star_measured_off_its_light_in_a_few_frames_makes_outliers() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), &mut sky(|_| {}), 120, |n, pixels, truth| {
            if n == 50 || n == 90 {
                move_brightest(pixels, truth, 4);
            }
        });
        let report = replay_of(dir.path(), 120).report;
        assert_eq!((report.outliers, report.deviations), (2, 960));
        assert!(report.failures.contains(&Failure::Outliers {
            outliers: 2,
            deviations: 960,
            max_fraction: 0.001
        }));
        assert!(report.stars[0].largest_deviation_px.unwrap() > 3.5);
    }

    #[test]
    fn an_empty_recording_measures_nothing_and_fails_on_too_few_frames() {
        let dir = tempfile::tempdir().unwrap();
        write_recording(dir.path(), 0, &[]);
        let replay = replay_of(dir.path(), 1);
        assert_eq!(replay.rows, []);
        assert_eq!(replay.report.stars, []);
        assert_eq!(
            replay.report.failures,
            [Failure::TooFewClear { frames: 0, min: 1 }]
        );
    }

    #[test]
    fn a_frame_with_nothing_to_measure_stops_the_replay() {
        let dir = tempfile::tempdir().unwrap();
        write_recording(dir.path(), 2, &[Some(Array2::from_elem((1, 5), 100))]);
        let recording = Recording::open(dir.path()).unwrap();
        let error = replay(&recording, StarParams::default(), tolerance(1)).unwrap_err();
        assert!(
            matches!(
                error,
                ReplayError::Frame {
                    frame: 1,
                    source: FrameError::Empty
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn the_rows_are_written_under_a_header() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), &mut sky(|_| {}), 1, |_, _, _| {});
        let replay = replay_of(dir.path(), 1);
        let mut out = Vec::new();
        write_csv(&replay.rows, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let mut lines = text.lines();
        assert_eq!(
            lines.next().unwrap(),
            "frame,start_utc,star,reading,x,y,offset_x,offset_y,deviation_x,deviation_y,\
             snr,hfd,flux,frame_offset_x,frame_offset_y,usable,star_lost,clouded"
        );
        let first = lines.next().unwrap();
        assert!(
            first.starts_with("1,2026-11-23T03:00:00.000Z,0,usable,"),
            "{first}"
        );
        assert!(first.ends_with(",8,false,"), "{first}");
        assert_eq!(lines.count(), 7);
    }

    #[test]
    fn a_star_s_scatter_and_outliers_are_measured_about_its_own_mean() {
        let tally = Tally {
            deviations: vec![[0.0, 1.0], [2.0, 1.0], [1.0, 4.0], [1.0, -2.0]],
            ..Tally::default()
        };
        let (star, outliers) = tally.report(3, 5, 1.0);
        assert_eq!((star.star, star.clear, star.usable), (3, 5, 4));
        assert_eq!(star.mean_deviation_px, Some([1.0, 1.0]));
        let [x, y] = star.scatter_px.unwrap();
        assert!((x - 0.5_f64.sqrt()).abs() < 1e-12, "{x}");
        assert!((y - 4.5_f64.sqrt()).abs() < 1e-12, "{y}");
        assert_eq!(star.largest_deviation_px, Some(3.0));
        assert_eq!(outliers, 4);
        assert_eq!(tally.report(3, 5, 3.0).1, 2);
        assert_eq!(tally.report(3, 5, 3.5).1, 0);
    }

    #[test]
    fn a_frame_whose_stars_all_brighten_past_the_flux_test_is_lost() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), &mut sky(|_| {}), 20, |n, pixels, _| {
            if n == 15 {
                pixels.mapv_inplace(|value| {
                    let above = f64::from(value) - 520.0;
                    (520.0 + 3.0 * above.max(0.0)).min(65_535.0) as u16
                });
            }
        });
        let replay = replay_of(dir.path(), 19);
        let lost: Vec<&StarRow> = replay.rows.iter().filter(|row| row.star_lost).collect();
        assert_eq!(lost.len(), 8);
        assert!(lost
            .iter()
            .all(|row| row.reading == Reading::FluxChanged && row.clouded == Some(false)));
        assert_eq!((replay.report.lost, replay.report.clouded), (1, 0));
    }
}

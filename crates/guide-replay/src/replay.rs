//! The replay: a recording's frames through the guider's star selection
//! and measurement. See `docs/crates/guide-replay.md` (The replay).

use std::io::Write;

use rp_guiding::{
    Frame, FrameError, FrameMeasurement, GuideStars, Offset, StarParams, StarReading, Usable,
};
use serde::Serialize;

use crate::presence::{self, Last};
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
    /// When it is not found or faint, whether it is there: a fresh star at
    /// its last place with its last flux.
    pub there: Option<bool>,
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
    /// Where it was last usable, and its flux there.
    last: Option<Last>,
    not_found: usize,
    faint: usize,
    flux_changed: usize,
    /// The frames that are not star-lost in which it is there and not
    /// usable.
    dropped: usize,
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
            dropped: self.dropped,
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
/// frame that yields the tolerance's fewest guide stars, measures every
/// frame after it, and holds what it finds to `tolerance`.
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
        } else if let Some((selected, measurement)) = GuideStars::select(&frame, params)
            .ok()
            .filter(|(_, measurement)| measurement.stars.len() >= tolerance.min_guide_stars)
        {
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
        let fresh = if needs_fresh(&measurement) {
            presence::fresh_stars(&frame, params)
        } else {
            Vec::new()
        };
        let verdict = account(&measurement, &fresh, &mut tallies, &mut report, params);
        rows.extend(rows_of(
            recorded.frame,
            &recorded.start_utc,
            &measurement,
            &verdict,
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

/// Whether a frame needs a fresh selection: it is star-lost, or a guide
/// star in it is not found or faint and may be there.
fn needs_fresh(measurement: &FrameMeasurement) -> bool {
    measurement.star_lost
        || measurement
            .stars
            .iter()
            .any(|reading| matches!(reading, StarReading::NotFound | StarReading::Faint { .. }))
}

/// What a measured frame shows beyond its readings.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Verdict {
    /// For each guide star, whether it is not found or faint while it is
    /// there.
    there: Vec<bool>,
    /// For a star-lost frame, whether it is clouded rather than lost.
    clouded: Option<bool>,
}

/// Accounts for a measured frame: which of its unused guide stars are
/// there among `fresh`, whether a star-lost frame is clouded or lost, and
/// each star's tally.
///
/// A star-lost frame is lost when a guide star is there, or when the
/// field jumped beyond the search region, and clouded otherwise: its
/// stars are gone, faint, or dimmed or brightened past the flux test,
/// because the sky changed their light.
fn account(
    measurement: &FrameMeasurement,
    fresh: &[Usable],
    tallies: &mut [Tally],
    report: &mut Report,
    params: StarParams,
) -> Verdict {
    let last: Vec<Option<Last>> = tallies.iter().map(|tally| tally.last).collect();
    let there: Vec<bool> = measurement
        .stars
        .iter()
        .zip(&last)
        .map(|(reading, last)| {
            matches!(reading, StarReading::NotFound | StarReading::Faint { .. })
                && presence::there(*last, fresh, params)
        })
        .collect();
    let clouded = measurement
        .star_lost
        .then(|| !(there.contains(&true) || presence::jumped(&last, fresh, params)));
    match clouded {
        Some(true) => report.clouded = report.clouded.saturating_add(1),
        Some(false) => report.lost = report.lost.saturating_add(1),
        None => report.clear = report.clear.saturating_add(1),
    }
    if let Some(frame_offset) = measurement.offset {
        for ((reading, tally), there) in
            measurement.stars.iter().zip(tallies.iter_mut()).zip(&there)
        {
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
            if *there {
                tally.dropped = tally.dropped.saturating_add(1);
            }
        }
    }
    for (reading, tally) in measurement.stars.iter().zip(tallies.iter_mut()) {
        if let StarReading::Usable(star) = reading {
            tally.last = Some(Last::of(star));
        }
    }
    Verdict { there, clouded }
}

fn rows_of<'a>(
    frame: u32,
    start_utc: &'a str,
    measurement: &'a FrameMeasurement,
    verdict: &'a Verdict,
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
                there: matches!(reading, StarReading::NotFound | StarReading::Faint { .. })
                    .then(|| verdict.there.get(star).copied().unwrap_or(false)),
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
                clouded: verdict.clouded,
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

    use guide_simulator::{Cloud, FrameTruth, PierSide, SkyOffset, StarField};
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
            [
                Failure::TooFewStars { min: 4 },
                Failure::TooFewClear { frames: 0, min: 1 }
            ]
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
            "frame,start_utc,star,reading,there,x,y,offset_x,offset_y,deviation_x,deviation_y,\
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
    fn a_frame_whose_stars_all_brighten_past_the_flux_test_is_clouded() {
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
            .all(|row| row.reading == Reading::FluxChanged && row.clouded == Some(true)));
        assert_eq!((replay.report.lost, replay.report.clouded), (0, 1));
    }

    #[test]
    fn a_thin_cloud_that_dims_the_bright_stars_and_hides_the_faint_is_clouded() {
        let dir = tempfile::tempdir().unwrap();
        let mut sim = sky(|config| {
            if let StarField::Listed(stars) = &mut config.sky.stars {
                for star in stars.iter_mut().skip(4) {
                    star.e_per_s *= 0.03;
                }
            }
            config.sky.clouds = vec![Cloud {
                start: Duration::from_secs(20),
                duration: Duration::from_secs(20),
                ramp: Duration::from_secs(2),
                transmission: 0.02,
            }];
        });
        record(dir.path(), &mut sim, 30, |_, _, _| {});
        let replay = replay_of(dir.path(), 20);
        let mixed = replay.rows.iter().any(|row| {
            row.star_lost
                && row.reading == Reading::NotFound
                && replay
                    .rows
                    .iter()
                    .any(|other| other.frame == row.frame && other.reading == Reading::FluxChanged)
        });
        assert!(mixed, "no star-lost frame mixes dimmed and missing stars");
        assert_eq!(replay.report.lost, 0, "{:?}", replay.report);
        assert!(replay.report.clouded >= 4, "{:?}", replay.report);
    }

    #[test]
    fn a_cloud_that_clears_after_the_flux_reference_followed_it_down_is_clouded() {
        let dir = tempfile::tempdir().unwrap();
        let mut sim = sky(|config| {
            config.sky.clouds = vec![Cloud {
                start: Duration::from_secs(20),
                duration: Duration::from_secs(60),
                ramp: Duration::from_secs(2),
                transmission: 0.4,
            }];
        });
        record(dir.path(), &mut sim, 45, |_, _, _| {});
        let replay = replay_of(dir.path(), 20);
        let brightened = replay
            .rows
            .iter()
            .any(|row| row.star_lost && row.reading == Reading::FluxChanged && row.frame > 32);
        assert!(brightened, "the clearing never tripped the flux test");
        assert_eq!(replay.report.lost, 0, "{:?}", replay.report);
    }

    /// Leaves only the brightest star's square of `pixels` and the floor
    /// elsewhere.
    fn keep_brightest(pixels: &mut Array2<u16>, truth: &FrameTruth) {
        let star = &truth.stars[0];
        let (x, y) = (
            star.position.x.round() as usize,
            star.position.y.round() as usize,
        );
        let window = pixels.slice(s![y - 8..y + 8, x - 8..x + 8]).to_owned();
        let floor = *pixels.iter().min().unwrap();
        pixels.fill(floor + 20);
        pixels
            .slice_mut(s![y - 8..y + 8, x - 8..x + 8])
            .assign(&window);
    }

    #[test]
    fn the_selection_waits_for_a_frame_with_enough_guide_stars() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), &mut sky(|_| {}), 6, |n, pixels, truth| {
            if n <= 2 {
                keep_brightest(pixels, truth);
            }
        });
        let report = replay_of(dir.path(), 4).report;
        assert_eq!((report.before_selection, report.measured), (2, 4));
        assert_eq!(report.stars.len(), 8);
        assert_eq!(report.failures, []);
    }

    #[test]
    fn a_recording_with_too_few_stars_to_select_fails() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), &mut sky(|_| {}), 3, |_, pixels, truth| {
            keep_brightest(pixels, truth);
        });
        let report = replay_of(dir.path(), 1).report;
        assert_eq!((report.before_selection, report.measured), (3, 0));
        assert!(report.failures.contains(&Failure::TooFewStars { min: 4 }));
    }

    fn usable_at(x: f64, y: f64, flux: f64, offset: Offset) -> Usable {
        Usable {
            position: rp_guiding::Position { x, y },
            offset,
            hfd: 3.0,
            snr: 50.0,
            flux,
        }
    }

    fn frame_of(stars: Vec<StarReading>, offset: Option<Offset>) -> FrameMeasurement {
        let usable = stars
            .iter()
            .filter(|reading| matches!(reading, StarReading::Usable(_)))
            .count();
        FrameMeasurement {
            offset,
            usable,
            star_lost: usable == 0,
            stars,
            median_hfd: None,
            median_snr: None,
            median_flux: None,
        }
    }

    fn tallies_at(stars: &[(f64, f64, f64)]) -> Vec<Tally> {
        stars
            .iter()
            .map(|(x, y, flux)| Tally {
                last: Some(Last::of(&usable_at(*x, *y, *flux, Offset::default()))),
                ..Tally::default()
            })
            .collect()
    }

    fn empty_report() -> Report {
        Report {
            frames: 0,
            failed: 0,
            before_selection: 0,
            measured: 0,
            clear: 0,
            clouded: 0,
            lost: 0,
            stars: Vec::new(),
            deviations: 0,
            outliers: 0,
            tolerance: Tolerance::default(),
            failures: Vec::new(),
        }
    }

    #[test]
    fn a_star_missing_in_a_clear_frame_while_it_is_there_is_dropped() {
        let params = StarParams::default();
        let mut tallies = tallies_at(&[(100.0, 100.0, 9_000.0), (300.0, 150.0, 6_000.0)]);
        let mut report = empty_report();
        let frame = frame_of(
            vec![
                StarReading::Usable(usable_at(101.0, 100.5, 9_100.0, Offset { x: 1.0, y: 0.5 })),
                StarReading::NotFound,
            ],
            Some(Offset { x: 1.0, y: 0.5 }),
        );
        let fresh = [usable_at(301.0, 150.0, 6_100.0, Offset::default())];
        let verdict = account(&frame, &fresh, &mut tallies, &mut report, params);
        assert_eq!(
            verdict,
            Verdict {
                there: vec![false, true],
                clouded: None
            }
        );
        assert_eq!((tallies[1].not_found, tallies[1].dropped), (1, 1));
        assert_eq!(tallies[0].deviations, [[0.0, 0.0]]);
        assert_eq!(tallies[0].last.unwrap().position.x, 101.0);
        assert_eq!(tallies[1].last.unwrap().position.x, 300.0);
        assert_eq!(report.clear, 1);
    }

    #[test]
    fn a_star_lost_frame_is_lost_while_a_guide_star_is_there_and_clouded_when_none_is() {
        let params = StarParams::default();
        let cases: [(StarReading, f64, Option<bool>); 4] = [
            (StarReading::NotFound, 6_000.0, Some(false)),
            (StarReading::Faint { snr: 2.0 }, 6_000.0, Some(false)),
            (StarReading::NotFound, 2_000.0, Some(true)),
            (
                StarReading::FluxChanged {
                    flux: 2_000.0,
                    reference: 6_000.0,
                },
                6_000.0,
                Some(true),
            ),
        ];
        for (reading, fresh_flux, clouded) in cases {
            let mut tallies = tallies_at(&[(100.0, 100.0, 9_000.0), (300.0, 150.0, 6_000.0)]);
            let mut report = empty_report();
            let frame = frame_of(vec![StarReading::NotFound, reading], None);
            let fresh = [usable_at(302.0, 151.0, fresh_flux, Offset::default())];
            let verdict = account(&frame, &fresh, &mut tallies, &mut report, params);
            assert_eq!(verdict.clouded, clouded, "{reading:?} with {fresh_flux}");
            assert_eq!(
                (report.lost, report.clouded),
                if clouded == Some(true) {
                    (0, 1)
                } else {
                    (1, 0)
                }
            );
            assert_eq!(tallies[1].dropped, 0, "a star-lost frame drops nothing");
        }
    }

    #[test]
    fn a_star_lost_frame_whose_field_jumped_is_lost() {
        let params = StarParams::default();
        let mut tallies = tallies_at(&[
            (100.0, 100.0, 9_000.0),
            (300.0, 150.0, 6_000.0),
            (200.0, 250.0, 4_000.0),
        ]);
        let mut report = empty_report();
        let frame = frame_of(vec![StarReading::NotFound; 3], None);
        let fresh = [
            usable_at(140.0, 130.0, 9_000.0, Offset::default()),
            usable_at(340.0, 180.0, 6_000.0, Offset::default()),
            usable_at(240.0, 280.0, 4_000.0, Offset::default()),
        ];
        let verdict = account(&frame, &fresh, &mut tallies, &mut report, params);
        assert_eq!(
            verdict,
            Verdict {
                there: vec![false; 3],
                clouded: Some(false)
            }
        );
    }

    #[test]
    fn a_fresh_selection_runs_for_a_star_lost_frame_or_an_unused_star() {
        let usable = StarReading::Usable(usable_at(1.0, 1.0, 1.0, Offset::default()));
        let changed = StarReading::FluxChanged {
            flux: 1.0,
            reference: 3.0,
        };
        let some = Some(Offset::default());
        assert!(!needs_fresh(&frame_of(vec![usable, usable], some)));
        assert!(!needs_fresh(&frame_of(vec![usable, changed], some)));
        assert!(needs_fresh(&frame_of(
            vec![usable, StarReading::NotFound],
            some
        )));
        assert!(needs_fresh(&frame_of(
            vec![usable, StarReading::Faint { snr: 2.0 }],
            some
        )));
        assert!(needs_fresh(&frame_of(vec![changed, changed], None)));
    }
}

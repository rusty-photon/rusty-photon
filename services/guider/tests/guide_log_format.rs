//! The guide log held to PHD2's format (docs/services/guider.md § Guide
//! log): the parser built from the logs PHD2 wrote on the rigs accepts each
//! of them, with what their README counts in them, and accepts a log the
//! guider's writer writes.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::indexing_slicing
)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![allow(
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::unused_async,
    clippy::unused_async_trait_impl,
    clippy::used_underscore_binding,
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::suboptimal_flops,
    clippy::too_many_lines,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::float_cmp,
    clippy::similar_names,
    clippy::struct_excessive_bools
)]

#[path = "bdd/phd2_log.rs"]
#[expect(
    dead_code,
    reason = "the parser reads every part of a log; each of its two users asserts on some"
)]
mod phd2_log;

use std::path::PathBuf;

use phd2_log::{header_value, parse, Log, Row, Section};

/// The fixtures, under the crate's directory as the test runs: Cargo sets
/// `CARGO_MANIFEST_DIR` for the test, and Bazel sets it to the crate's
/// path in the test's runfiles.
fn fixtures() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")), PathBuf::from)
        .join("tests/fixtures/guide-logs")
}

fn fixture(name: &str) -> Log {
    let text = std::fs::read_to_string(fixtures().join(name)).unwrap();
    parse(&text).unwrap_or_else(|error| panic!("{name}: {error}"))
}

fn calibrations(log: &Log) -> Vec<&phd2_log::Calibration> {
    log.blocks
        .iter()
        .flat_map(|block| &block.sections)
        .filter_map(|section| match section {
            Section::Calibration(calibration) => Some(calibration),
            _ => None,
        })
        .collect()
}

fn runs(log: &Log) -> Vec<&phd2_log::Guiding> {
    log.blocks
        .iter()
        .flat_map(|block| &block.sections)
        .filter_map(|section| match section {
            Section::Guiding(guiding) => Some(guiding),
            _ => None,
        })
        .collect()
}

#[test]
fn every_fixture_parses() {
    let mut names: Vec<_> = std::fs::read_dir(fixtures())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| {
            std::path::Path::new(name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("txt"))
        })
        .collect();
    names.sort();
    assert_eq!(names.len(), 5);
    for name in names {
        fixture(&name);
    }
}

#[test]
fn a_short_night_parses_to_what_the_readme_counts_in_it() {
    let log = fixture("PHD2_GuideLog_2025-05-13_002522.txt");
    let calibrations = calibrations(&log);
    assert_eq!(calibrations.len(), 1);
    let rows = |direction: &str| {
        calibrations[0]
            .rows
            .iter()
            .filter(|row| row.direction == direction)
            .count()
    };
    assert_eq!(
        [
            rows("West"),
            rows("East"),
            rows("Backlash"),
            rows("North"),
            rows("South")
        ],
        [21, 3, 8, 13, 4]
    );
    assert_eq!(calibrations[0].legs[0].angle_deg, 10.5);
    assert_eq!(calibrations[0].legs[1].rate, 18.225);
    let runs = runs(&log);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs.iter().map(|run| run.rows.len()).sum::<usize>(), 690);
    assert_eq!(
        header_value(&runs[0].header, "Pixel scale"),
        Some("0.78 arc-sec/px")
    );
    assert_eq!(header_value(&runs[0].header, "xAngle"), Some("10.5"));
}

#[test]
fn the_night_with_the_most_dropped_frames_parses_to_what_the_readme_counts_in_it() {
    let log = fixture("PHD2_GuideLog_2025-06-17_231857.txt");
    let rows: Vec<&Row> = runs(&log).iter().flat_map(|run| &run.rows).collect();
    let drops = rows
        .iter()
        .filter(|row| matches!(row, Row::Drop(_)))
        .count();
    assert_eq!((runs(&log).len(), rows.len(), drops), (29, 4_584, 1_526));
}

/// A short block with one run, `row` its only row.
fn with_row(row: &str) -> String {
    format!(
        "PHD2 version 2.6.13 [Windows], Log version 2.5. Log enabled at 2025-05-13 00:25:22\n\
         \n\
         Guiding Begins at 2025-05-13 00:31:51\n\
         Pixel scale = 0.78 arc-sec/px, Binning = 1, Focal length = 530 mm\n\
         Frame,Time,mount,dx,dy,RARawDistance,DECRawDistance,RAGuideDistance,DECGuideDistance,RADuration,RADirection,DECDuration,DECDirection,XStep,YStep,StarMass,SNR,ErrorCode\n\
         {row}\n\
         Guiding Ends at 2025-05-13 00:32:37\n\
         \n\
         Log Summary: calcnt:0 gcnt:1 gdur:46 gacnt:0\n\
         Log closed at 2025-05-13 01:10:23\n"
    )
}

#[test]
fn the_parser_takes_a_row_of_the_format() {
    let row =
        r#"2,7.265,"Mount",-0.730,-0.345,-0.781,-0.246,-0.586,0.000,54,E,0,,,,489143,110.27,0"#;
    parse(&with_row(row)).unwrap();
}

#[test]
fn the_parser_refuses_what_the_format_does_not_hold() {
    for (row, why) in [
        (
            r#"2,7.265,"Mount",-0.73,-0.345,-0.781,-0.246,-0.586,0.000,54,E,0,,,,489143,110.27,0"#,
            "two decimals where the format has three",
        ),
        (
            r#"2,7.265,"Mount",-0.730,-0.345,-0.781,-0.246,-0.586,0.000,54,,0,,,,489143,110.27,0"#,
            "a pulse without its direction",
        ),
        (
            r#"2,7.265,"Mount",-0.730,-0.345,-0.781,-0.246,-0.586,0.000,0,E,0,,,,489143,110.27,0"#,
            "a direction without a pulse",
        ),
        (
            r#"2,7.265,"Mount",-0.730,-0.345,-0.781,-0.246,-0.586,0.000,54,N,0,,,,489143,110.27,0"#,
            "a Dec direction on the RA pulse",
        ),
        (
            r#"2,7.265,"DROP",,,,,,,,,,,,0,0.00,2,"Star lost - low SNR""#,
            "a dropped frame with a field missing",
        ),
    ] {
        assert!(parse(&with_row(row)).is_err(), "{why} was taken");
    }
    let miscounted = with_row(
        r#"2,7.265,"Mount",-0.730,-0.345,-0.781,-0.246,-0.586,0.000,54,E,0,,,,489143,110.27,0"#,
    )
    .replace("gcnt:1", "gcnt:2");
    assert!(
        parse(&miscounted).is_err(),
        "a summary miscounting the runs was taken"
    );
    let unclosed = with_row(
        r#"2,7.265,"Mount",-0.730,-0.345,-0.781,-0.246,-0.586,0.000,54,E,0,,,,489143,110.27,0"#,
    )
    .replace("Log closed at 2025-05-13 01:10:23\n", "");
    assert!(
        parse(&unclosed).is_err(),
        "a block that does not close was taken"
    );
}

mod writer {
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use chrono::{DateTime, FixedOffset, TimeZone as _};
    use guider::config::GuideLogConfig;
    use guider::guide_log::{CalibrationRow, Common, Control, GuideLog, GuideRow, Pointing, DIR};
    use rp_guiding::{
        Axis, AxisOffset, Calibration, CameraGeometry, DecAxis, Direction, GuideRates, Leg, Offset,
        PierSide, Position, Pulse, Setup,
    };

    use super::phd2_log::{header_value, parse, Log, Row, Section};

    /// A clock that stands still until a test moves it, in an hour east of
    /// Greenwich.
    fn clock(
        start: &str,
    ) -> (
        Arc<AtomicI64>,
        Box<dyn Fn() -> DateTime<FixedOffset> + Send + Sync>,
    ) {
        let zone = FixedOffset::east_opt(3_600).unwrap();
        let start = DateTime::parse_from_rfc3339(start).unwrap().timestamp();
        let now = Arc::new(AtomicI64::new(start));
        let read = Arc::clone(&now);
        let clock = Box::new(move || zone.timestamp_opt(read.load(Ordering::SeqCst), 0).unwrap());
        (now, clock)
    }

    const fn calibration() -> Calibration {
        Calibration {
            setup: Setup {
                pier_side: PierSide::East,
                camera: CameraGeometry {
                    pixel_size_um: 2.0,
                    width: 320,
                    height: 240,
                    binning: 1,
                },
                focal_length_mm: 206.265,
                guide_rates: Some(GuideRates {
                    ra_deg_per_s: 0.004_178,
                    dec_deg_per_s: 0.004_178,
                }),
            },
            declination_deg: 0.0,
            ra: Axis {
                angle_deg: 0.0,
                rate_px_per_s: 7.52,
                steps: 4,
                step: Duration::from_millis(200),
            },
            dec: Some(DecAxis {
                axis: Axis {
                    angle_deg: 270.0,
                    rate_px_per_s: 7.52,
                    steps: 4,
                    step: Duration::from_millis(200),
                },
                backlash: Duration::ZERO,
            }),
            warnings: Vec::new(),
        }
    }

    fn common() -> Common {
        Common {
            pixel_scale: 2.0,
            binning: 1,
            focal_length_mm: 206.265,
            search_region_px: 15.0,
            mass_tolerance: 0.5,
            stars: 3,
            camera: "Alpaca Camera 0".to_owned(),
            gain: None,
            width: 320,
            height: 240,
            pixel_size_um: 2.0,
            exposure: Duration::from_millis(200),
            mount: "Alpaca Telescope 0".to_owned(),
            guide_rates: Some(GuideRates {
                ra_deg_per_s: 0.004_178,
                dec_deg_per_s: 0.004_178,
            }),
            pointing: Some(Pointing {
                ra_hours: 6.0,
                altitude_deg: 45.0,
                azimuth_deg: 180.0,
                sidereal_hours: 7.5,
            }),
            declination_deg: 0.0,
            pier_side: PierSide::East,
            lock: Position { x: 100.0, y: 80.0 },
            star: Position { x: 100.0, y: 80.0 },
            hfd: Some(3.1),
        }
    }

    const fn control() -> Control {
        Control {
            ra_hysteresis: 0.1,
            ra_aggression: 0.7,
            dec_aggression: 1.0,
            dec_reversal_frames: 3,
            dec_mode: Some("Auto"),
            min_move_px: 0.229,
            max_pulse: Duration::from_millis(2_500),
        }
    }

    fn row(leg: Leg, step: u32, x: f64) -> CalibrationRow {
        CalibrationRow {
            leg,
            step,
            start: Offset::default(),
            now: Offset { x, y: 0.0 },
            star: Position {
                x: 100.0 + x,
                y: 80.0,
            },
        }
    }

    const fn pulse(direction: Direction, ms: u64) -> Pulse {
        Pulse {
            direction,
            duration: Duration::from_millis(ms),
        }
    }

    const fn guide_row(ra: Option<Pulse>, dec: Option<Pulse>) -> GuideRow {
        GuideRow {
            from_lock: Offset { x: 0.5, y: -0.25 },
            error: AxisOffset { ra: 0.5, dec: 0.25 },
            ra,
            dec,
            ra_rate_px_per_s: 7.52,
            dec_rate_px_per_s: 7.52,
            flux: Some(50_000.4),
            snr: Some(42.126),
        }
    }

    fn only_log(state: &std::path::Path) -> (String, Log) {
        let mut files: Vec<_> = std::fs::read_dir(state.join(DIR))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1, "{files:?}");
        let path = files.pop().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let log = parse(&text).unwrap_or_else(|error| panic!("{error}\n{text}"));
        (name, log)
    }

    #[test]
    fn a_night_of_the_guiders_parses_as_phd2s_format() {
        let state = tempfile::tempdir().unwrap();
        let (now, clock) = clock("2026-11-25T21:00:00+01:00");
        let mut log = GuideLog::with_clock(state.path(), GuideLogConfig::default(), clock);
        log.begin_calibration(&common(), Duration::from_millis(200), 6.0);
        log.calibration_row(&row(Leg::West, 0, 0.0));
        log.calibration_row(&row(Leg::West, 1, -1.5));
        log.leg_complete(Leg::West, 0.0, 7.52);
        log.calibration_row(&row(Leg::East, 1, -1.5));
        log.calibration_row(&row(Leg::ClearBacklash, 0, 0.0));
        log.calibration_star_lost(None, None);
        log.calibration_row(&row(Leg::North, 1, 0.0));
        log.leg_complete(Leg::North, 270.0, 7.52);
        log.calibration_row(&row(Leg::South, 1, 0.0));
        log.calibration_complete("Alpaca Telescope 0");
        let at = |secs: f64| Duration::from_secs_f64(secs);
        log.begin_guiding(&common(), &calibration(), &control(), at(100.0));
        log.note("SETTLING STATE CHANGE, Settling started");
        log.guide_step(
            &guide_row(
                Some(pulse(Direction::East, 47)),
                Some(pulse(Direction::South, 33)),
            ),
            at(101.25),
        );
        log.guide_step(
            &guide_row(Some(pulse(Direction::West, 47)), None),
            at(102.5),
        );
        log.guide_step(
            &guide_row(None, Some(pulse(Direction::North, 33))),
            at(103.75),
        );
        log.guide_step(&guide_row(None, None), at(105.0));
        log.note("SETTLING STATE CHANGE, Settling complete");
        log.star_lost(None, None, at(106.25));
        log.note("DITHER by 1.000, -2.000, new lock pos = 101.000, 78.000");
        log.note("Server received guiding/pause, full = false");
        log.note("Server received guiding/resume");
        now.fetch_add(60, Ordering::SeqCst);
        log.stop_requested(at(160.0));
        log.begin_calibration(&common(), Duration::from_millis(200), 6.0);
        log.calibration_row(&row(Leg::West, 0, 0.0));
        log.calibration_failed(
            "the calibration failed: the West leg had not moved the stars far enough",
        );
        log.close();

        let (name, log) = only_log(state.path());
        assert_eq!(name, "PHD2_GuideLog_2026-11-25_210000.txt");
        assert_eq!(log.blocks.len(), 1);
        let block = &log.blocks[0];
        assert!(
            block.writer.starts_with("rusty-photon guider "),
            "{}",
            block.writer
        );
        assert_eq!(block.enabled_at, "2026-11-25 21:00:00");
        let summary = block.summary.unwrap();
        assert_eq!((summary.calcnt, summary.gcnt, summary.gdur), (1, 1, 60));
        let [Section::Calibration(passed), Section::Guiding(run), Section::Calibration(failed)] =
            block.sections.as_slice()
        else {
            panic!("{:?}", block.sections);
        };
        let directions: Vec<&str> = passed
            .rows
            .iter()
            .map(|row| row.direction.as_str())
            .collect();
        assert_eq!(
            directions,
            ["West", "West", "East", "Backlash", "North", "South"]
        );
        assert_eq!(
            passed.rows[1].dx, 1.5,
            "the displacement is the way the mount moved"
        );
        let legs: Vec<(&str, f64)> = passed
            .legs
            .iter()
            .map(|leg| (leg.direction.as_str(), leg.angle_deg))
            .collect();
        assert_eq!(legs, [("West", -180.0), ("North", 90.0)]);
        assert_eq!(passed.complete.as_deref(), Some("Alpaca Telescope 0"));
        assert!(passed.notes[0].starts_with("STAR LOST during calibration"));
        assert_eq!(
            header_value(&run.header, "Pixel scale"),
            Some("2.00 arc-sec/px")
        );
        assert_eq!(header_value(&run.header, "xAngle"), Some("-180.0"));
        assert_eq!(header_value(&run.header, "yAngle"), Some("90.0"));
        assert_eq!(header_value(&run.header, "Hour angle"), Some("1.50 hr"));
        assert_eq!(header_value(&run.header, "Max RA duration"), Some("2500"));
        assert_eq!(header_value(&run.header, "Minimum move"), Some("0.229"));
        let Row::Mount(first) = &run.rows[0] else {
            panic!("{:?}", run.rows[0]);
        };
        assert_eq!((first.frame, first.time), (1, 1.25));
        assert_eq!(
            (first.ra_raw, first.dec_raw),
            (-0.5, 0.25),
            "signed as PHD2 signs them"
        );
        assert_eq!(
            (first.ra_direction, first.dec_direction),
            (Some('E'), Some('S'))
        );
        assert_eq!(
            (first.ra_guide, first.star_mass, first.snr),
            (-0.353, 50_000, 42.13)
        );
        let Row::Drop(dropped) = &run.rows[4] else {
            panic!("{:?}", run.rows[4]);
        };
        assert_eq!(
            (dropped.code, dropped.reason.as_str()),
            (5, "Too few guide stars usable")
        );
        let notes: Vec<&str> = run.notes.iter().map(|(_, note)| note.as_str()).collect();
        assert_eq!(
            notes,
            [
                "SETTLING STATE CHANGE, Settling started",
                "SETTLING STATE CHANGE, Settling complete",
                "DITHER by 1.000, -2.000, new lock pos = 101.000, 78.000",
                "Server received guiding/pause, full = false",
                "Server received guiding/resume",
                "Server received guiding/stop",
            ]
        );
        assert_eq!(run.ends_at.as_deref(), Some("2026-11-25 21:01:00"));
        assert_eq!(failed.complete, None);
        assert!(
            failed.notes[0].starts_with("CALIBRATION FAILED, "),
            "{:?}",
            failed.notes
        );
    }

    #[test]
    fn the_first_run_after_local_noon_opens_the_next_nights_file() {
        let state = tempfile::tempdir().unwrap();
        let (now, clock) = clock("2026-11-25T23:00:00+01:00");
        let mut log = GuideLog::with_clock(state.path(), GuideLogConfig::default(), clock);
        log.begin_calibration(&common(), Duration::from_millis(200), 6.0);
        log.calibration_failed("given up");
        // 11:59 the next morning is still the night's.
        now.fetch_add(12 * 3_600 + 59 * 60, Ordering::SeqCst);
        log.begin_calibration(&common(), Duration::from_millis(200), 6.0);
        log.calibration_failed("given up");
        let first = log.path().unwrap().to_owned();
        now.fetch_add(60, Ordering::SeqCst);
        log.begin_calibration(&common(), Duration::from_millis(200), 6.0);
        let second = log.path().unwrap().to_owned();
        log.close();
        assert_ne!(first, second);
        assert!(
            second.ends_with("PHD2_GuideLog_2026-11-26_120000.txt"),
            "{second:?}"
        );
        for path in [first, second] {
            parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        }
    }

    #[test]
    fn opening_a_file_removes_the_files_older_than_the_retention() {
        let state = tempfile::tempdir().unwrap();
        let dir = state.path().join(DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("PHD2_GuideLog_2026-08-01_210000.txt");
        let kept = dir.join("PHD2_GuideLog_2026-11-01_210000.txt");
        let other = dir.join("notes.txt");
        for path in [&old, &kept, &other] {
            std::fs::write(path, "x").unwrap();
        }
        let days = |days: u64| std::time::SystemTime::now() - Duration::from_hours(days * 24);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(days(100))
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&other)
            .unwrap()
            .set_modified(days(100))
            .unwrap();
        let mut log = GuideLog::new(state.path(), GuideLogConfig::default());
        log.begin_calibration(&common(), Duration::from_millis(200), 6.0);
        log.close();
        assert!(!old.exists(), "the file of 100 days ago was kept");
        assert!(kept.exists() && other.exists());
    }

    #[test]
    fn a_log_that_cannot_be_written_is_off_until_the_next_nights_file() {
        let state = tempfile::tempdir().unwrap();
        // A file where the directory would be.
        std::fs::write(state.path().join(DIR), "not a directory").unwrap();
        let (now, clock) = clock("2026-11-25T21:00:00+01:00");
        let mut log = GuideLog::with_clock(state.path(), GuideLogConfig::default(), clock);
        log.begin_calibration(&common(), Duration::from_millis(200), 6.0);
        log.calibration_row(&row(Leg::West, 0, 0.0));
        assert!(log.path().is_none());
        std::fs::remove_file(state.path().join(DIR)).unwrap();
        log.begin_calibration(&common(), Duration::from_millis(200), 6.0);
        assert!(
            log.path().is_none(),
            "the log came back before the next file"
        );
        now.fetch_add(16 * 3_600, Ordering::SeqCst);
        log.begin_calibration(&common(), Duration::from_millis(200), 6.0);
        assert!(log.path().is_some(), "the next night's file did not open");
    }

    #[test]
    fn a_log_the_service_closed_opens_no_file_after() {
        let state = tempfile::tempdir().unwrap();
        let mut log = GuideLog::new(state.path(), GuideLogConfig::default());
        log.begin_calibration(&common(), Duration::from_millis(200), 6.0);
        log.close();
        log.begin_calibration(&common(), Duration::from_millis(200), 6.0);
        assert!(log.path().is_none());
        assert_eq!(
            std::fs::read_dir(state.path().join(DIR)).unwrap().count(),
            1
        );
    }

    #[test]
    fn a_log_that_is_off_writes_nothing() {
        let state = tempfile::tempdir().unwrap();
        let config = GuideLogConfig {
            enabled: false,
            ..GuideLogConfig::default()
        };
        let mut log = GuideLog::new(state.path(), config);
        log.begin_calibration(&common(), Duration::from_millis(200), 6.0);
        log.close();
        assert!(!state.path().join(DIR).exists());
    }
}

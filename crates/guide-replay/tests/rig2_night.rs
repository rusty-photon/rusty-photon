//! The hour rig2 recorded on 2026-11-22, as 21 frames cropped from it,
//! replayed through the guider's measurement and held to stage G3's
//! tolerance, the count of frames apart. `guide-replay crop` cut them:
//! every 72nd frame of the hour, 400 × 300 px from (1100, 200) of the
//! 678M's 2 × 2 binned frame. The record is
//! `docs/validation/2026-11-22-guide-replay-qhy5iii678m-rig2/`.

use std::path::PathBuf;

use guide_replay::{replay, Recording, Tolerance};
use rp_guiding::StarParams;

/// The fixtures, under the crate's directory as the test runs: Cargo sets
/// `CARGO_MANIFEST_DIR` for the test, and Bazel sets it to the crate's
/// path in the test's runfiles.
fn fixtures() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")), PathBuf::from)
        .join("tests/fixtures/rig2-2026-11-22")
}

#[test]
fn the_hour_on_rig2_holds_its_guide_stars_and_they_agree() {
    let recording = Recording::open(&fixtures()).unwrap();
    assert_eq!(recording.len(), 21);
    let tolerance = Tolerance {
        min_clear_frames: 21,
        ..Tolerance::default()
    };
    let report = replay(&recording, StarParams::default(), tolerance)
        .unwrap()
        .report;
    assert_eq!(report.failures, [], "{report}");
    assert_eq!(
        (report.measured, report.clear, report.clouded, report.lost),
        (21, 21, 0, 0)
    );
    assert_eq!(report.stars.len(), 8);
    for star in &report.stars {
        assert_eq!(star.usable, 21, "star {}", star.star);
        let [x, y] = star.scatter_px.unwrap();
        assert!(x < 0.25 && y < 0.25, "star {}: {x} {y}", star.star);
    }
    assert_eq!(report.outliers, 0);
}

//! By-hand verification helper for the `auto_focus` V-curve fixtures.
//!
//! Loads each FITS file under
//! `services/rp/tests/fixtures/auto_focus/`, runs `measure_basic`
//! against it with the same `threshold/min_area/max_area` defaults the
//! BDD scenario will use, and prints the detected HFR + star count
//! next to the expected HFR. Used to validate fixture quality before
//! locking them in as the canonical V-curve test inputs.
//!
//! Run: `cargo run --example check_autofocus_fixtures -p rp`

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
// The expected-HFR polynomial mirrors the fixture generator's shape.
#![allow(clippy::suboptimal_flops)]

use std::path::PathBuf;

use ndarray::Array2;
use rp::imaging::measure_basic;
use rp::persistence::read_fits_pixels;

const OFFSETS: [i32; 11] = [-100, -80, -60, -40, -20, 0, 20, 40, 60, 80, 100];

fn expected_hfr(d: i32) -> f64 {
    2.0 + 0.0005 * f64::from(d).powi(2)
}

fn fixture_path(d: i32) -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = manifest_dir
        .join("tests")
        .join("fixtures")
        .join("auto_focus");
    let name = if d < 0 {
        format!("pos_m{:03}.fits", d.unsigned_abs())
    } else {
        format!("pos_p{:03}.fits", d.cast_unsigned())
    };
    dir.join(name)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "{:>6} {:>10} {:>10} {:>8} {:>10}",
        "offset", "expected", "measured", "stars", "Δ"
    );
    let mut max_abs_delta: f64 = 0.0;
    for &d in &OFFSETS {
        let path = fixture_path(d);
        let (pixels, w, h) = read_fits_pixels(&path)?;
        let arr: Array2<i32> = Array2::from_shape_vec((w, h), pixels)?;
        // Same parameters the BDD will use:
        //   threshold_sigma=5.0  (default)
        //   min_area=4
        //   max_area=2000        (cover the ~1200 px² "above threshold"
        //                         region of an HFR=7 star — see
        //                         gen_autofocus_fixtures.rs comments)
        //   max_adu=Some(65535)  (matches u16 sensor / fixture writer)
        let result =
            measure_basic(&arr.view(), 5.0, 4, 2000, Some(65535)).expect("measure_basic failed");
        let measured = result.hfr.unwrap_or(f64::NAN);
        let expected = expected_hfr(d);
        let delta = measured - expected;
        max_abs_delta = max_abs_delta.max(delta.abs());
        println!(
            "{:>+6} {:>10.4} {:>10.4} {:>8} {:>+10.4}",
            d, expected, measured, result.star_count, delta
        );
    }
    println!();
    println!("max |Δ| = {max_abs_delta:.4} px");
    Ok(())
}

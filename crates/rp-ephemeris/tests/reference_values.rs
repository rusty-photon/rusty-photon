//! Reference-value test: compare `ErfarsEphemeris`'s `alt_az` against
//! Astropy's values in `refvals/*.json`, within 1e-4°.
//!
//! `refvals/gen.py`, run by hand, writes those files, and they are
//! committed (see the README in that directory). The test cross-validates
//! the crate against an independent implementation, which the unit tests
//! do not, and fails when it finds no file to check.

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

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use rp_ephemeris::{Ephemeris, ErfarsEphemeris, IcrsCoord, Site};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct AltAzRef {
    altitude_degrees: f64,
    azimuth_degrees: f64,
}

#[derive(Debug, Deserialize)]
struct TargetEntry {
    ra_hours: f64,
    dec_degrees: f64,
    alt_az: AltAzRef,
}

#[derive(Debug, Deserialize)]
struct SiteRef {
    latitude_degrees: f64,
    longitude_degrees: f64,
}

#[derive(Debug, Deserialize)]
struct RefvalsFile {
    site: SiteRef,
    time_utc: String,
    time_label: String,
    targets: HashMap<String, TargetEntry>,
}

/// `refvals/`, under the crate's directory as the test runs: Cargo sets
/// `CARGO_MANIFEST_DIR` for the test, and Bazel sets it to the crate's
/// path in the test's runfiles.
fn refvals_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")), PathBuf::from)
        .join("refvals")
}

#[test]
fn alt_az_matches_astropy_within_tight_tolerance() {
    // Astropy uses ERFA internally and `gen.py` passes the same
    // 1013.25 mbar / 10 °C / 50 % / 0.55 µm refraction model, and the same
    // zero ΔUT1 and polar motion, that `ErfarsEphemeris` hard-codes, so
    // end-to-end agreement should be near-perfect. 1e-4 ° = 0.36″ is well below any meaningful
    // pointing tolerance and far above the f64 numerical noise the
    // two paths could plausibly accumulate; tighter than this would
    // start tripping on the last bit of double-precision rounding.
    const ALT_TOL_DEG: f64 = 1e-4;
    const AZ_TOL_DEG: f64 = 1e-4;

    let dir = refvals_dir();
    let entries: Vec<_> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        })
        .collect();
    assert!(
        !entries.is_empty(),
        "no refvals/*.json in {}: run refvals/gen.py and commit its output",
        dir.display()
    );

    let eph = ErfarsEphemeris::new();
    for entry in entries {
        let body = fs::read_to_string(entry.path()).unwrap();
        let refs: RefvalsFile = serde_json::from_str(&body).unwrap();
        let site = Site::new(refs.site.latitude_degrees, refs.site.longitude_degrees).unwrap();
        let when: DateTime<Utc> = refs.time_utc.parse().unwrap();
        for (name, target) in refs.targets {
            // Each entry carries the catalog ICRS place `gen.py` gave
            // Astropy, which the trait's alt_az takes as given.
            let coords = IcrsCoord {
                ra_hours: target.ra_hours,
                dec_degrees: target.dec_degrees,
            };
            let aa = eph.alt_az(&site, coords, when).unwrap();
            assert!(
                (aa.altitude_degrees - target.alt_az.altitude_degrees).abs() < ALT_TOL_DEG,
                "alt mismatch for {name} at {} ({}): erfars={:.6}°, astropy={:.6}°",
                refs.time_label,
                refs.time_utc,
                aa.altitude_degrees,
                target.alt_az.altitude_degrees
            );
            assert!(
                (aa.azimuth_degrees - target.alt_az.azimuth_degrees).abs() < AZ_TOL_DEG,
                "az mismatch for {name} at {} ({}): erfars={:.6}°, astropy={:.6}°",
                refs.time_label,
                refs.time_utc,
                aa.azimuth_degrees,
                target.alt_az.azimuth_degrees
            );
        }
    }
}

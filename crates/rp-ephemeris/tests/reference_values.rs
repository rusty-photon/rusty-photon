//! Reference-value tests: compare `ErfarsEphemeris`'s `alt_az` against
//! Astropy's values in `refvals/*.json`, within 1e-4°, and its
//! `moon_position`'s alt/az against Astropy's topocentric Moon, within 10″.
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
struct MoonRef {
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
    moon: MoonRef,
}

/// `refvals/`, under the crate's directory as the test runs: Cargo sets
/// `CARGO_MANIFEST_DIR` for the test, and Bazel sets it to the crate's
/// path in the test's runfiles.
fn refvals_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")), PathBuf::from)
        .join("refvals")
}

/// Every `refvals/*.json`, parsed, or why not: an error names the file,
/// and finding none is an error too.
fn load_refvals() -> Result<Vec<RefvalsFile>, String> {
    let dir = refvals_dir();
    let files = fs::read_dir(&dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        })
        .map(|path| {
            let body = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            serde_json::from_str(&body).map_err(|e| format!("{}: {e}", path.display()))
        })
        .collect::<Result<Vec<RefvalsFile>, String>>()?;
    if files.is_empty() {
        return Err(format!(
            "no refvals/*.json in {}: run refvals/gen.py and commit its output",
            dir.display()
        ));
    }
    Ok(files)
}

/// Great-circle angle between two alt/az directions, in degrees, by the
/// haversine formula, which keeps its precision at small angles.
fn separation_degrees(alt1: f64, az1: f64, alt2: f64, az2: f64) -> f64 {
    let (p1, p2) = (alt1.to_radians(), alt2.to_radians());
    let half_dp = (p2 - p1) / 2.0;
    let half_dl = (az2 - az1).to_radians() / 2.0;
    let h = half_dp.sin().powi(2) + p1.cos() * p2.cos() * half_dl.sin().powi(2);
    2.0 * h.sqrt().asin().to_degrees()
}

#[test]
fn alt_az_matches_astropy_within_tight_tolerance() {
    // Astropy uses ERFA internally and `gen.py` passes the same
    // 1013.25 mbar / 10 °C / 50 % / 0.55 µm refraction model, and the same
    // zero ΔUT1 and polar motion, that `ErfarsEphemeris` hard-codes; the
    // committed values agree with ERFA to about 1e-11°. 1e-4° = 0.36″ is
    // far below any pointing tolerance, and its margin over that agreement
    // is room for an Astropy or ERFA point release to move the last digits.
    const ALT_TOL_DEG: f64 = 1e-4;
    const AZ_TOL_DEG: f64 = 1e-4;

    let eph = ErfarsEphemeris::new();
    for refs in load_refvals().unwrap() {
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

#[test]
fn moon_alt_az_matches_astropy_de440s_within_10_arcsec() {
    // `gen.py` takes the Moon from JPL's DE440s, seen from the site, so the
    // disagreement here is algorithmic: `Moon98` is a truncated lunar
    // theory, and agrees with DE440s to about 4″ at these times. 10″ leaves
    // room for that, and is far below the Moon's parallax, which moves it by
    // at least half a degree at every one of these times.
    const TOL_ARCSEC: f64 = 10.0;

    let eph = ErfarsEphemeris::new();
    for refs in load_refvals().unwrap() {
        let site = Site::new(refs.site.latitude_degrees, refs.site.longitude_degrees).unwrap();
        let when: DateTime<Utc> = refs.time_utc.parse().unwrap();
        let ours = eph.moon_position(&site, when).alt_az;
        let theirs = &refs.moon.alt_az;
        let off_arcsec = separation_degrees(
            ours.altitude_degrees,
            ours.azimuth_degrees,
            theirs.altitude_degrees,
            theirs.azimuth_degrees,
        ) * 3600.0;
        assert!(
            off_arcsec < TOL_ARCSEC,
            "Moon {off_arcsec:.2}\" from Astropy at {} ({}), lat {} lon {}: \
             erfars alt {:.5}° az {:.5}°, astropy alt {:.5}° az {:.5}°",
            refs.time_label,
            refs.time_utc,
            refs.site.latitude_degrees,
            refs.site.longitude_degrees,
            ours.altitude_degrees,
            ours.azimuth_degrees,
            theirs.altitude_degrees,
            theirs.azimuth_degrees
        );
    }
}

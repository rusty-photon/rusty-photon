//! Reference-value tests: compare `ErfarsEphemeris` against canonical
//! Astropy-computed values committed under `refvals/*.json`.
//!
//! The JSON files are produced by hand via `refvals/gen.py` (see the
//! README in that directory). If no JSON files are present (e.g. on a
//! fresh checkout where the contributor has not yet run the script),
//! the test is a no-op — the unit tests in the crate cover correctness;
//! these tests cover *cross-validation* against an independent
//! implementation.

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

fn refvals_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("refvals")
}

#[test]
fn alt_az_matches_astropy_within_tight_tolerance() {
    // Astropy uses ERFA internally and `gen.py` passes the same
    // 1013.25 mbar / 10 °C / 50 % / 0.55 µm refraction model that
    // `ErfarsEphemeris` hard-codes, so end-to-end agreement should
    // be near-perfect. 1e-4 ° = 0.36″ is well below any meaningful
    // pointing tolerance and far above the f64 numerical noise the
    // two paths could plausibly accumulate; tighter than this would
    // start tripping on the last bit of double-precision rounding.
    const ALT_TOL_DEG: f64 = 1e-4;
    const AZ_TOL_DEG: f64 = 1e-4;

    let dir = refvals_dir();
    let entries: Vec<_> = match fs::read_dir(&dir) {
        Ok(it) => it
            .filter_map(Result::ok)
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            })
            .collect(),
        Err(_) => return, // refvals dir absent — nothing to check
    };

    if entries.is_empty() {
        // Treat empty refvals/ as a soft skip. The README documents the
        // gen.py workflow; CI relying solely on this assertion would
        // hide refvals erosion. An audit gate that fails if entries==0
        // for *too long* belongs in the project plan tracking, not in
        // a unit test.
        eprintln!("no refvals/*.json present at {dir:?} — run refvals/gen.py to populate");
        return;
    }

    let eph = ErfarsEphemeris::new();
    for entry in entries {
        let body = fs::read_to_string(entry.path()).unwrap();
        let refs: RefvalsFile = serde_json::from_str(&body).unwrap();
        let site = Site::new(refs.site.latitude_degrees, refs.site.longitude_degrees).unwrap();
        let when: DateTime<Utc> = refs.time_utc.parse().unwrap();
        for (name, target) in refs.targets {
            // Sun + Moon entries embed their own ICRS in the file —
            // for fixed stars that's just the catalog entry. Either
            // way the trait's alt_az takes ICRS as given.
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

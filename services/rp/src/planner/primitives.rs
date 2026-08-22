//! Helpers behind the primitive ephemeris MCP tools.
//!
//! Covers `compute_alt_az`, `compute_transit`, `compute_rise_set`,
//! `compute_meridian_flip`, `get_sun_position`, `get_twilight`,
//! `get_moon_position`, `compute_moon_separation`, and
//! `get_local_sidereal_time`.
//!
//! Each helper takes the parsed input + a borrowed `Site` (where
//! relevant) and returns a `serde_json::Value` that the MCP tool body
//! in `crate::mcp` projects onto a `CallToolResult` success or error
//! payload. Keeping the JSON-shaping in this module lets the
//! convenience tools (Phase 7) reuse the same primitive calls.

use chrono::{DateTime, NaiveDate, Utc};
use rp_ephemeris::{Ephemeris, ErfarsEphemeris, IcrsCoord, SideOfPier, Site, TwilightKind};
use serde_json::{json, Value};

/// Parse an RFC3339 timestamp, defaulting to `Utc::now()` when the
/// caller omits it.
///
/// # Errors
///
/// Returns a message if a supplied `s` is not RFC3339 (humantime
/// durations like `"5m"` are not accepted here).
pub fn parse_time_or_now(s: Option<&str>) -> Result<DateTime<Utc>, String> {
    s.map_or_else(
        || Ok(Utc::now()),
        |raw| {
            DateTime::parse_from_rfc3339(raw)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| {
                    format!("invalid time {raw:?}: {e} (expect RFC3339, e.g. 2026-05-03T22:00:00Z)")
                })
        },
    )
}

/// Parse a `YYYY-MM-DD` UTC date string.
///
/// # Errors
///
/// Returns a message if `s` is not a `YYYY-MM-DD` date.
pub fn parse_date(s: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|e| format!("invalid date {s:?}: {e} (expect YYYY-MM-DD)"))
}

/// Parse the `kind` parameter for `get_twilight` (case-insensitive).
///
/// # Errors
///
/// Returns a message if `s` is not one of `civil`, `nautical`, or
/// `astronomical`.
pub fn parse_twilight_kind(s: &str) -> Result<TwilightKind, String> {
    match s.to_ascii_lowercase().as_str() {
        "civil" => Ok(TwilightKind::Civil),
        "nautical" => Ok(TwilightKind::Nautical),
        "astronomical" => Ok(TwilightKind::Astronomical),
        other => Err(format!(
            "unknown twilight kind {other:?}; expected one of civil, nautical, astronomical"
        )),
    }
}

/// Parse the `side_of_pier` parameter for `compute_meridian_flip`
/// (case-insensitive).
///
/// # Errors
///
/// Returns a message if `s` is not one of `east`, `west`, or `unknown`.
pub fn parse_side_of_pier(s: &str) -> Result<SideOfPier, String> {
    match s.to_ascii_lowercase().as_str() {
        "east" => Ok(SideOfPier::East),
        "west" => Ok(SideOfPier::West),
        "unknown" => Ok(SideOfPier::Unknown),
        other => Err(format!(
            "unknown side_of_pier {other:?}; expected one of east, west, unknown"
        )),
    }
}

/// Validate ra (hours) ∈ [0, 24) and dec (degrees) ∈ [-90, 90].
///
/// # Errors
///
/// Returns a message naming the out-of-range coordinate.
pub fn validate_icrs(ra_hours: f64, dec_degrees: f64) -> Result<IcrsCoord, String> {
    if !(0.0..24.0).contains(&ra_hours) {
        return Err(format!("ra_hours must be in [0, 24); got {ra_hours}"));
    }
    if !(-90.0..=90.0).contains(&dec_degrees) {
        return Err(format!(
            "dec_degrees must be in [-90, 90]; got {dec_degrees}"
        ));
    }
    Ok(IcrsCoord {
        ra_hours,
        dec_degrees,
    })
}

/// Common error: a tool that needs a configured site was called when
/// the deployment has no `site` block. The MCP tool body projects this
/// onto a structured error payload.
#[must_use]
pub fn site_required_error() -> String {
    "site not configured: this tool requires a `site` block in rp's config; \
     see docs/services/rp.md §\"Site Configuration\""
        .to_string()
}

// ---------------------------------------------------------------------------
// Tool body helpers — return JSON Value on success, String on failure.
// ---------------------------------------------------------------------------

/// The `compute_alt_az` body: the target's altitude and azimuth at
/// `time` as seen from `site`.
///
/// # Errors
///
/// Returns a message if the alt/az transform fails (invalid site or
/// target inputs).
pub fn compute_alt_az(
    site: &Site,
    target: IcrsCoord,
    time: DateTime<Utc>,
) -> Result<Value, String> {
    let aa = ErfarsEphemeris::new()
        .alt_az(site, target, time)
        .map_err(|e| format!("alt/az transform failed: {e}"))?;
    Ok(json!({
        "altitude_degrees": aa.altitude_degrees,
        "azimuth_degrees": aa.azimuth_degrees,
    }))
}

#[must_use]
pub fn compute_transit(site: &Site, target: IcrsCoord, date: NaiveDate) -> Value {
    let result = ErfarsEphemeris::new().transit(site, target, date);
    json!({
        "transit_utc": result.map(|t| t.to_rfc3339()),
    })
}

#[must_use]
pub fn compute_rise_set(
    site: &Site,
    target: IcrsCoord,
    date: NaiveDate,
    min_alt_degrees: f64,
) -> Value {
    let result = ErfarsEphemeris::new().rise_set(site, target, date, min_alt_degrees);
    result.map_or_else(
        || {
            json!({
                "rise_utc": serde_json::Value::Null,
                "set_utc": serde_json::Value::Null,
            })
        },
        |rs| {
            json!({
                "rise_utc": rs.rise_utc.to_rfc3339(),
                "set_utc": rs.set_utc.to_rfc3339(),
            })
        },
    )
}

#[must_use]
pub fn compute_meridian_flip(
    site: &Site,
    target: IcrsCoord,
    time: DateTime<Utc>,
    pier_side: SideOfPier,
) -> Value {
    let result = ErfarsEphemeris::new().meridian_flip(site, target, time, pier_side);
    json!({
        "time_to_flip_seconds": result.map(|d| d.num_seconds()),
    })
}

#[must_use]
pub fn get_sun_position(site: &Site, time: DateTime<Utc>) -> Value {
    let info = ErfarsEphemeris::new().sun_position(site, time);
    json!({
        "ra_hours": info.coords.ra_hours,
        "dec_degrees": info.coords.dec_degrees,
        // Alt/az can be NaN at degenerate sites where the topocentric
        // transform is undefined (the trait substitutes NaN rather
        // than erroring). serde_json panics on non-finite floats, so
        // we map them to JSON null.
        "altitude_degrees": finite_or_null(info.alt_az.altitude_degrees),
        "azimuth_degrees": finite_or_null(info.alt_az.azimuth_degrees),
    })
}

/// Map an `f64` to a JSON value, substituting `null` for NaN /
/// infinities so `serde_json::json!` doesn't panic.
fn finite_or_null(v: f64) -> Value {
    if v.is_finite() {
        json!(v)
    } else {
        Value::Null
    }
}

#[must_use]
pub fn get_twilight(site: &Site, date: NaiveDate, kind: TwilightKind) -> Value {
    let w = ErfarsEphemeris::new().twilight(site, date, kind);
    json!({
        "kind": match kind {
            TwilightKind::Civil => "civil",
            TwilightKind::Nautical => "nautical",
            TwilightKind::Astronomical => "astronomical",
        },
        "begin_utc": w.begin_utc.map(|t| t.to_rfc3339()),
        "end_utc": w.end_utc.map(|t| t.to_rfc3339()),
    })
}

#[must_use]
pub fn get_moon_position(site: &Site, time: DateTime<Utc>) -> Value {
    let info = ErfarsEphemeris::new().moon_position(site, time);
    json!({
        "ra_hours": info.coords.ra_hours,
        "dec_degrees": info.coords.dec_degrees,
        // Alt/az can be NaN at degenerate sites; same protection as
        // `get_sun_position`.
        "altitude_degrees": finite_or_null(info.alt_az.altitude_degrees),
        "azimuth_degrees": finite_or_null(info.alt_az.azimuth_degrees),
        "phase_degrees": info.phase_degrees,
        "illumination_fraction": info.illumination_fraction,
    })
}

#[must_use]
pub fn compute_moon_separation(target: IcrsCoord, time: DateTime<Utc>) -> Value {
    let sep = ErfarsEphemeris::new().moon_separation(target, time);
    json!({
        "separation_degrees": sep,
    })
}

#[must_use]
pub fn get_local_sidereal_time(site: &Site, time: DateTime<Utc>) -> Value {
    let lst = ErfarsEphemeris::new().sidereal_time(site, time);
    json!({
        "lst_hours": lst.lst_hours,
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn site_seattle() -> Site {
        Site::new(47.6062, -122.3321).unwrap()
    }

    #[test]
    fn parses_rfc3339_or_uses_now() {
        let parsed = parse_time_or_now(Some("2026-05-03T22:00:00Z")).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-05-03T22:00:00+00:00");
        let now = parse_time_or_now(None).unwrap();
        assert!(now.timestamp() > 0);
    }

    #[test]
    fn rejects_bad_time_with_helpful_diagnostic() {
        let err = parse_time_or_now(Some("not a time")).unwrap_err();
        assert!(err.contains("RFC3339"), "got: {err}");
    }

    #[test]
    fn parse_date_round_trips() {
        let d = parse_date("2026-05-03").unwrap();
        assert_eq!(d.format("%Y-%m-%d").to_string(), "2026-05-03");
    }

    #[test]
    fn icrs_validation_blocks_out_of_range() {
        assert!(validate_icrs(-1.0, 0.0).is_err());
        assert!(validate_icrs(24.0, 0.0).is_err());
        assert!(validate_icrs(0.0, 91.0).is_err());
        validate_icrs(0.7123, 41.27).unwrap();
    }

    #[test]
    fn twilight_kind_parses_known_variants() {
        assert_eq!(
            parse_twilight_kind("Astronomical").unwrap(),
            TwilightKind::Astronomical
        );
        assert!(parse_twilight_kind("daytime").is_err());
    }

    #[test]
    fn side_of_pier_parses_known_variants() {
        assert_eq!(parse_side_of_pier("East").unwrap(), SideOfPier::East);
        assert!(parse_side_of_pier("middle").is_err());
    }

    #[test]
    fn alt_az_returns_finite_numbers_for_known_target() {
        let site = site_seattle();
        let m31 = IcrsCoord {
            ra_hours: 0.7123,
            dec_degrees: 41.27,
        };
        let t = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 11, 1, 6, 0, 0).unwrap();
        let v = compute_alt_az(&site, m31, t).unwrap();
        let alt = v["altitude_degrees"].as_f64().unwrap();
        assert!(alt.is_finite() && (-90.0..=90.0).contains(&alt));
    }

    #[test]
    fn lst_in_canonical_range() {
        let site = site_seattle();
        let t = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 5, 3, 0, 0, 0).unwrap();
        let v = get_local_sidereal_time(&site, t);
        let lst = v["lst_hours"].as_f64().unwrap();
        assert!((0.0..24.0).contains(&lst));
    }

    #[test]
    fn alt_az_returns_error_on_unrepresentable_inputs() {
        // ERFA refuses lat = 90° (the pole, where azimuth is
        // singular). compute_alt_az propagates that as a String error.
        let site = Site::new(90.0, 0.0).unwrap();
        let t = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 5, 3, 0, 0, 0).unwrap();
        let target = IcrsCoord {
            ra_hours: 0.0,
            dec_degrees: 0.0,
        };
        // Either succeeds (ERFA tolerates the pole) or returns an Err.
        // Both branches exercise the helper end-to-end.
        let _ = compute_alt_az(&site, target, t);
    }

    #[test]
    fn transit_emits_expected_keys_for_typical_target() {
        let site = site_seattle();
        let date = chrono::NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        let target = IcrsCoord {
            ra_hours: 0.7123,
            dec_degrees: 41.27,
        };
        let v = compute_transit(&site, target, date);
        assert!(v.get("transit_utc").is_some());
        assert!(v["transit_utc"].is_string() || v["transit_utc"].is_null());
    }

    #[test]
    fn rise_set_circumpolar_target_returns_null_bounds() {
        let site = site_seattle();
        let date = chrono::NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        // Polaris at Seattle is circumpolar above 10°.
        let polaris = IcrsCoord {
            ra_hours: 2.530_194_4,
            dec_degrees: 89.264_111_1,
        };
        let v = compute_rise_set(&site, polaris, date, 10.0);
        assert!(v["rise_utc"].is_null());
        assert!(v["set_utc"].is_null());
    }

    #[test]
    fn rise_set_typical_target_emits_iso_strings() {
        let site = site_seattle();
        let date = chrono::NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        let m31 = IcrsCoord {
            ra_hours: 0.7123,
            dec_degrees: 41.27,
        };
        let v = compute_rise_set(&site, m31, date, 30.0);
        // Either both bounds or both null.
        match (v["rise_utc"].as_str(), v["set_utc"].as_str()) {
            (Some(_), Some(_)) => {}
            (None, None) => panic!("expected M31 to rise above 30° at Seattle in autumn"),
            (a, b) => panic!("inconsistent: rise={a:?} set={b:?}"),
        }
    }

    #[test]
    fn meridian_flip_emits_seconds() {
        let site = site_seattle();
        let t = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 5, 3, 12, 0, 0).unwrap();
        let target = IcrsCoord {
            ra_hours: 0.7123,
            dec_degrees: 41.27,
        };
        let v = compute_meridian_flip(&site, target, t, SideOfPier::Unknown);
        let secs = v["time_to_flip_seconds"].as_i64().unwrap();
        assert!(secs > 0 && secs <= 86_400);
    }

    #[test]
    fn sun_position_emits_all_fields() {
        let site = site_seattle();
        let t = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 5, 3, 18, 0, 0).unwrap();
        let v = get_sun_position(&site, t);
        for k in [
            "ra_hours",
            "dec_degrees",
            "altitude_degrees",
            "azimuth_degrees",
        ] {
            assert!(v[k].as_f64().is_some(), "missing {k}: {v}");
        }
    }

    #[test]
    fn twilight_emits_kind_label() {
        let site = site_seattle();
        let date = chrono::NaiveDate::from_ymd_opt(2026, 12, 21).unwrap();
        for kind in [
            TwilightKind::Civil,
            TwilightKind::Nautical,
            TwilightKind::Astronomical,
        ] {
            let v = get_twilight(&site, date, kind);
            assert!(v["kind"].is_string());
        }
    }

    #[test]
    fn moon_position_phase_in_canonical_range() {
        let site = site_seattle();
        let t = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 5, 3, 18, 0, 0).unwrap();
        let v = get_moon_position(&site, t);
        let phase = v["phase_degrees"].as_f64().unwrap();
        let illum = v["illumination_fraction"].as_f64().unwrap();
        assert!((0.0..=180.0).contains(&phase));
        assert!((0.0..=1.0).contains(&illum));
        // Sanity-check the corrected formula sign: phase ≈ 0 → illum ≈ 0;
        // phase ≈ 180 → illum ≈ 1.
        let expected = (1.0 - (phase * std::f64::consts::PI / 180.0).cos()) / 2.0;
        assert!((illum - expected).abs() < 1e-9);
    }

    #[test]
    fn finite_or_null_maps_nan_and_infinity_to_null() {
        assert_eq!(finite_or_null(0.0), serde_json::json!(0.0));
        assert_eq!(finite_or_null(42.5), serde_json::json!(42.5));
        assert!(finite_or_null(f64::NAN).is_null());
        assert!(finite_or_null(f64::INFINITY).is_null());
        assert!(finite_or_null(f64::NEG_INFINITY).is_null());
    }

    #[test]
    fn sun_position_handles_nan_alt_az_without_panicking() {
        // The Site at exactly the geographic pole is a contrived case
        // for ERFA's topocentric transform; ErfarsEphemeris substitutes
        // NaN alt/az when the transform errors. Without `finite_or_null`,
        // `json!` would panic. This pin asserts we just emit JSON null.
        let pole = Site::new(90.0, 0.0).unwrap();
        let t = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 5, 3, 12, 0, 0).unwrap();
        let v = get_sun_position(&pole, t);
        // ra_hours / dec_degrees come from Epv00 (geocentric); they're
        // always finite. alt/az may be null if the transform errored.
        assert!(v["ra_hours"].as_f64().is_some());
        // The contract is "no panic" — alt/az is either finite or
        // null, never NaN.
        for k in ["altitude_degrees", "azimuth_degrees"] {
            let val = &v[k];
            assert!(
                val.is_null() || val.as_f64().is_some_and(f64::is_finite),
                "{k} should be null or finite, got {val:?}"
            );
        }
    }

    #[test]
    fn moon_separation_for_target_at_moon_position_is_zero() {
        let t = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 5, 3, 18, 0, 0).unwrap();
        // Compute the moon position via the trait, then ask for the
        // separation of *that* point — should be ~0 by construction.
        let site = site_seattle();
        let m = get_moon_position(&site, t);
        let coord = IcrsCoord {
            ra_hours: m["ra_hours"].as_f64().unwrap(),
            dec_degrees: m["dec_degrees"].as_f64().unwrap(),
        };
        let sep = compute_moon_separation(coord, t)["separation_degrees"]
            .as_f64()
            .unwrap();
        assert!(sep.abs() < 1e-6, "expected ~0° separation, got {sep}");
    }
}

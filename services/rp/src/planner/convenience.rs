//! Convenience-tool body helpers: `get_target_status` and
//! `get_meridian_status`. Each produces a JSON `Value` from typed
//! inputs through a derived `Serialize` projection — there is no
//! hand-built JSON here. (`get_next_target` has no helper at all: its
//! wire type is `super::decision::NextTargetRecommendation`, whose own
//! derived `Serialize` the tool body serializes directly.) The MCP tool
//! body in `crate::mcp` handles parameter parsing, equipment lookup,
//! deriving progress, and `CallToolResult` shaping.
//!
//! The per-filter `{completed, goal}` progress views that used to live
//! here are gone with the counters: every progress surface now emits
//! the per-goal rows `crate::mcp::built_in::targets::progress_rows`
//! builds from the on-disk scan (rp.md § Progress derivation), so there
//! is one definition of a target's progress rather than two shapes that
//! could drift.

use chrono::{DateTime, Utc};
use rp_ephemeris::{Ephemeris, ErfarsEphemeris, IcrsCoord, SideOfPier, Site};
use serde::Serialize;
use serde_json::Value;

use super::decision::signed_hour_angle;

/// The `get_target_status` result: sky position for one named target
/// plus the caller-supplied `progress` (the per-filter map when the
/// name matches a configured target, `null` otherwise).
#[derive(Serialize)]
struct TargetStatusView<'a> {
    target_name: &'a str,
    altitude_degrees: f64,
    azimuth_degrees: f64,
    hour_angle_hours: f64,
    /// `null` when the target is circumpolar above `min_altitude` or
    /// never reaches it on the supplied date.
    time_to_set_seconds: Option<i64>,
    progress: Value,
}

/// Status of a single named target: alt, az, hour-angle, time-to-set,
/// plus the caller-supplied `progress` (passed through verbatim).
pub fn target_status_view(
    site: &Site,
    target: IcrsCoord,
    target_name: &str,
    now: DateTime<Utc>,
    min_altitude_degrees: f64,
    progress: Value,
) -> Result<Value, String> {
    let eph = ErfarsEphemeris::new();
    let aa = eph
        .alt_az(site, target, now)
        .map_err(|e| format!("alt/az transform failed: {e}"))?;
    let lst = eph.sidereal_time(site, now).lst_hours;
    let ha = signed_hour_angle(lst, target.ra_hours);

    // `rise_set` answers for transits within a single UTC date. A
    // target that was up at the start of the UTC day (transit happened
    // late on the previous calendar day) reports a `set_utc` that may
    // already be in the past for *today's* date — what the caller
    // actually wants is "the next set, possibly from yesterday's
    // visibility window". Try today; if its set is in the past or
    // missing, look at the previous UTC date too.
    let today = now.date_naive();
    let yesterday = today.pred_opt();
    let pick_set = |rs: Option<rp_ephemeris::RiseSet>| {
        rs.and_then(|r| {
            if r.set_utc > now {
                Some(r.set_utc.signed_duration_since(now).num_seconds())
            } else {
                None
            }
        })
    };
    let today_rs = eph.rise_set(site, target, today, min_altitude_degrees);
    let time_to_set_seconds = pick_set(today_rs).or_else(|| {
        yesterday.and_then(|d| pick_set(eph.rise_set(site, target, d, min_altitude_degrees)))
    });

    let view = TargetStatusView {
        target_name,
        altitude_degrees: aa.altitude_degrees,
        azimuth_degrees: aa.azimuth_degrees,
        hour_angle_hours: ha,
        time_to_set_seconds,
        progress,
    };
    Ok(serde_json::to_value(view).unwrap_or(Value::Null))
}

/// The `get_meridian_status` result: time-to-flip from the mount's
/// current pointing plus the side of pier (`SideOfPier`'s own
/// kebab-case `Serialize` gives `"east"` / `"west"` / `"unknown"`).
#[derive(Serialize)]
struct MeridianStatusView {
    time_to_flip_seconds: Option<i64>,
    side_of_pier: SideOfPier,
    mount_ra_hours: f64,
    mount_dec_degrees: f64,
}

/// Status of the meridian-flip clock: time-to-flip from the mount's
/// current pointing, plus the side of pier.
#[must_use]
pub fn meridian_status_view(
    site: &Site,
    mount_ra_hours: f64,
    mount_dec_degrees: f64,
    now: DateTime<Utc>,
    side: SideOfPier,
) -> Value {
    let eph = ErfarsEphemeris::new();
    let target = IcrsCoord {
        ra_hours: mount_ra_hours,
        dec_degrees: mount_dec_degrees,
    };
    let dur = eph.meridian_flip(site, target, now, side);
    let view = MeridianStatusView {
        time_to_flip_seconds: dur.map(|d| d.num_seconds()),
        side_of_pier: side,
        mount_ra_hours,
        mount_dec_degrees,
    };
    serde_json::to_value(view).unwrap_or(Value::Null)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn site() -> Site {
        Site::new(47.6062, -122.3321).unwrap()
    }

    #[test]
    fn target_status_for_polaris_emits_expected_fields() {
        let polaris = IcrsCoord {
            ra_hours: 2.5301944,
            dec_degrees: 89.2641111,
        };
        let now = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 11, 1, 6, 0, 0).unwrap();
        let v = target_status_view(&site(), polaris, "Polaris", now, 20.0, Value::Null).unwrap();
        assert_eq!(v["target_name"], "Polaris");
        assert!(v["altitude_degrees"].as_f64().is_some());
        assert!(v["azimuth_degrees"].as_f64().is_some());
        assert!(v["hour_angle_hours"].as_f64().is_some());
        // Polaris is circumpolar at Seattle, so time_to_set_seconds
        // is null.
        assert!(v["time_to_set_seconds"].is_null());
        // The progress argument passes through verbatim (null here —
        // Polaris is not a configured target).
        assert!(v["progress"].is_null());
    }

    #[test]
    fn meridian_status_view_includes_side_of_pier() {
        let now = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 11, 1, 6, 0, 0).unwrap();
        let v = meridian_status_view(&site(), 12.0, 0.0, now, SideOfPier::East);
        assert_eq!(v["side_of_pier"], "east");
        assert!(v["time_to_flip_seconds"].as_i64().is_some());
    }
}

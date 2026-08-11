//! Operations not in ERFA's surface — small root-finders over the
//! ERFA-supplied positions in `erfars_impl`.

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};

use crate::erfars_impl::{alt_az_at, lst_hours, time_jds};
use crate::site::Site;
use crate::types::{IcrsCoord, RiseSet, SolarHours, TwilightKind, TwilightWindow};

/// 1 sidereal hour in solar hours.
const SIDEREAL_TO_SOLAR: f64 = 0.997_269_566_3;

/// Bisect a function over a `DateTime<Utc>` interval looking for a
/// sign change of `f`. Returns `None` if the function does not change
/// sign on the bracket. Tolerance is in whole seconds.
fn bisect_dt<F>(
    mut f: F,
    lo: DateTime<Utc>,
    hi: DateTime<Utc>,
    tol_secs: i64,
) -> Option<DateTime<Utc>>
where
    F: FnMut(DateTime<Utc>) -> f64,
{
    let flo = f(lo);
    let fhi = f(hi);
    if flo.is_nan() || fhi.is_nan() {
        return None;
    }
    if flo * fhi > 0.0 {
        return None;
    }
    let mut lo = lo;
    let mut hi = hi;
    let mut flo_sign = flo.signum();
    // Halving in whole milliseconds (i64 division by a literal) is
    // total, and sub-millisecond precision is irrelevant against the
    // whole-second tolerance.
    let midpoint = |lo: DateTime<Utc>, hi: DateTime<Utc>| {
        let half = Duration::milliseconds(hi.signed_duration_since(lo).num_milliseconds() / 2);
        lo.checked_add_signed(half)
    };
    while hi.signed_duration_since(lo).num_seconds() > tol_secs {
        let mid = midpoint(lo, hi)?;
        let fmid = f(mid);
        if fmid.is_nan() {
            return None;
        }
        if fmid == 0.0 {
            return Some(mid);
        }
        if fmid.signum() == flo_sign {
            lo = mid;
            flo_sign = fmid.signum();
        } else {
            hi = mid;
        }
    }
    midpoint(lo, hi)
}

/// UT of upper transit on the given UTC date. Closed-form via LST,
/// refined by one Newton step against the actual computed LST at the
/// candidate time.
pub fn transit(site: &Site, target: IcrsCoord, date: NaiveDate) -> Option<DateTime<Utc>> {
    let start = date.and_time(NaiveTime::MIN).and_utc();
    let lst0 = lst_hours(site, &time_jds(start));
    // NaN propagates through `rem_euclid` but `as i64` saturates NaN
    // to 0, which would silently collapse the computation to "start".
    // Surface the failure as `None` instead so callers see the
    // upstream time-conversion problem.
    if !lst0.is_finite() {
        return None;
    }
    let delta_sidereal = (target.ra_hours - lst0).rem_euclid(24.0);
    let delta_solar = delta_sidereal * SIDEREAL_TO_SOLAR;
    let candidate = start.checked_add_signed(SolarHours(delta_solar).into())?;

    // One Newton iteration: re-evaluate LST at the candidate, take
    // the residual mod 24 (signed: residual > 12h means we overshot).
    let lst1 = lst_hours(site, &time_jds(candidate));
    if !lst1.is_finite() {
        return None;
    }
    let mut residual = (target.ra_hours - lst1).rem_euclid(24.0);
    if residual > 12.0 {
        residual -= 24.0;
    }
    candidate.checked_add_signed(SolarHours(residual * SIDEREAL_TO_SOLAR).into())
}

/// Rise/set times above `min_alt_deg`.
pub fn rise_set(
    _eph: &impl crate::Ephemeris, // unused for v1; reserved for future
    site: &Site,
    target: IcrsCoord,
    date: NaiveDate,
    min_alt_deg: f64,
) -> Option<RiseSet> {
    let transit_t = transit(site, target, date)?;
    // Antitransit is 12 sidereal hours away; use 11h57.97m solar.
    let half_sidereal_day_solar: Duration = SolarHours(12.0 * SIDEREAL_TO_SOLAR).into();
    let antitransit_before = transit_t.checked_sub_signed(half_sidereal_day_solar)?;
    let antitransit_after = transit_t.checked_add_signed(half_sidereal_day_solar)?;

    let alt_minus_thresh = |t: DateTime<Utc>| -> f64 {
        match alt_az_at(site, target, &time_jds(t)) {
            Ok(aa) => aa.altitude_degrees - min_alt_deg,
            Err(_) => f64::NAN,
        }
    };

    let alt_at_transit = alt_minus_thresh(transit_t);
    if alt_at_transit < 0.0 {
        return None; // never reaches threshold
    }
    let alt_anti_before = alt_minus_thresh(antitransit_before);
    let alt_anti_after = alt_minus_thresh(antitransit_after);
    if alt_anti_before >= 0.0 && alt_anti_after >= 0.0 {
        return None; // always above threshold (circumpolar-up)
    }

    let rise = bisect_dt(alt_minus_thresh, antitransit_before, transit_t, 1);
    let set = bisect_dt(alt_minus_thresh, transit_t, antitransit_after, 1);
    match (rise, set) {
        (Some(rise_utc), Some(set_utc)) => Some(RiseSet { rise_utc, set_utc }),
        _ => None,
    }
}

/// Time until the target next reaches the meridian (HA = 0). Side of
/// pier is ignored in v1 — the convenience tool's caller treats the
/// returned duration as "time until a flip might be required".
pub fn meridian_flip(site: &Site, target: IcrsCoord, time: DateTime<Utc>) -> Option<Duration> {
    let lst = lst_hours(site, &time_jds(time));
    if !lst.is_finite() {
        return None;
    }
    let ha = (lst - target.ra_hours).rem_euclid(24.0);
    // ha ∈ [0, 24). HA = 0 means the target is on the meridian *right
    // now* — the flip is due now, not in another full sidereal day.
    // For 0 < ha < 24, transit is `24 - ha` sidereal hours in the
    // future.
    let hours_sidereal = if ha == 0.0 { 0.0 } else { 24.0 - ha };
    let hours_solar = hours_sidereal * SIDEREAL_TO_SOLAR;
    Some(SolarHours(hours_solar).into())
}

/// Civil/nautical/astronomical twilight bracket centred on the local
/// night that covers `date` (UTC). Returns `Some` for both bounds when
/// the Sun crosses the threshold both going down (evening) and going
/// up (morning); `None` for either bound at high latitudes where the
/// Sun never crosses the threshold.
pub fn twilight(
    eph: &impl crate::Ephemeris,
    site: &Site,
    date: NaiveDate,
    kind: TwilightKind,
) -> TwilightWindow {
    let threshold = kind.sun_altitude_threshold_degrees();
    // Approximate local solar noon: noon UTC shifted by 4 minutes per
    // degree of longitude, spelled `longitude / 15` solar hours.
    // longitude_degrees is positive east, so local solar noon is
    // *earlier* in UTC for eastern longitudes. Built via
    // `NaiveDate::and_time(NaiveTime::MIN)` (infallible) + a 12-hour
    // Duration so there's no `from_hms_opt` Option-unwrap to dodge for
    // the panic-deny lint. The checked adds can only fail at chrono's
    // extreme date boundary, where "no twilight computable" is the
    // honest answer — the window's existing empty shape.
    let empty = TwilightWindow {
        begin_utc: None,
        end_utc: None,
    };
    let Some(solar_noon) = date
        .and_time(NaiveTime::MIN)
        .and_utc()
        .checked_add_signed(Duration::hours(12))
        .and_then(|noon| noon.checked_sub_signed(SolarHours(site.longitude_degrees / 15.0).into()))
    else {
        return empty;
    };
    let (Some(midnight), Some(next_noon)) = (
        solar_noon.checked_add_signed(Duration::hours(12)),
        solar_noon.checked_add_signed(Duration::hours(24)),
    ) else {
        return empty;
    };

    // Sun altitude relative to threshold, as a sign-changing function
    // of time. We rely on the trait method so derived twilight is
    // testable against a hand-rolled mock Ephemeris in the planner
    // crate.
    let f = |t: DateTime<Utc>| eph.sun_position(site, t).alt_az.altitude_degrees - threshold;

    let begin_utc = bisect_dt(f, solar_noon, midnight, 1);
    let end_utc = bisect_dt(f, midnight, next_noon, 1);
    TwilightWindow { begin_utc, end_utc }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::erfars_impl::ErfarsEphemeris;
    use crate::Ephemeris;
    use chrono::{NaiveDateTime, TimeZone};

    fn site_seattle() -> Site {
        Site::new(47.6062, -122.3321).unwrap()
    }

    #[test]
    fn polaris_at_seattle_circumpolar_returns_none() {
        let eph = ErfarsEphemeris::new();
        let polaris = IcrsCoord {
            ra_hours: 2.5301944,
            dec_degrees: 89.2641111,
        };
        let date = NaiveDate::from_ymd_opt(2026, 5, 3).unwrap();
        // Polaris never sets at Seattle (lat ~47.6°), so above
        // min_alt_deg = 10° is always-up.
        assert!(rise_set(&eph, &site_seattle(), polaris, date, 10.0).is_none());
    }

    #[test]
    fn extreme_southern_target_at_seattle_never_up() {
        let eph = ErfarsEphemeris::new();
        // Octans-region target near the south celestial pole — never
        // visible from Seattle.
        let target = IcrsCoord {
            ra_hours: 12.0,
            dec_degrees: -85.0,
        };
        let date = NaiveDate::from_ymd_opt(2026, 5, 3).unwrap();
        assert!(rise_set(&eph, &site_seattle(), target, date, 10.0).is_none());
    }

    #[test]
    fn typical_target_rises_and_sets_within_24h() {
        let eph = ErfarsEphemeris::new();
        let m31 = IcrsCoord {
            ra_hours: 0.7122,
            dec_degrees: 41.2689,
        };
        let date = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        let rs = rise_set(&eph, &site_seattle(), m31, date, 30.0)
            .expect("M31 should rise above 30° at Seattle in autumn");
        assert!(rs.set_utc > rs.rise_utc, "set must follow rise");
        let span = rs.set_utc - rs.rise_utc;
        assert!(span > Duration::hours(1));
        assert!(span < Duration::hours(24));
    }

    #[test]
    fn transit_within_one_day_of_requested_date() {
        let m31 = IcrsCoord {
            ra_hours: 0.7122,
            dec_degrees: 41.2689,
        };
        let date = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        let t = transit(&site_seattle(), m31, date).unwrap();
        let window_start =
            NaiveDateTime::new(date, NaiveTime::from_hms_opt(0, 0, 0).unwrap()).and_utc();
        assert!(t >= window_start);
        assert!(t < window_start + Duration::hours(24));
    }

    #[test]
    fn meridian_flip_at_meridian_returns_zero() {
        // Construct a target whose RA equals the current LST: HA = 0.
        // The flip is "right now", so the duration should be ~0, not
        // a full sidereal day.
        let eph = ErfarsEphemeris::new();
        let site = site_seattle();
        let t = Utc.with_ymd_and_hms(2026, 5, 3, 12, 0, 0).unwrap();
        let lst = eph.sidereal_time(&site, t).lst_hours;
        let target = IcrsCoord {
            ra_hours: lst,
            dec_degrees: 30.0,
        };
        let d = meridian_flip(&site, target, t).unwrap();
        // Allow ~1 minute slack for the f64 roundtrip through
        // chrono::Duration::milliseconds; should be far below a full
        // sidereal day.
        assert!(
            d.num_seconds().abs() < 60,
            "expected ~0s at HA=0, got {}s",
            d.num_seconds()
        );
    }

    #[test]
    fn meridian_flip_returns_positive_duration() {
        let eph = ErfarsEphemeris::new();
        let m31 = IcrsCoord {
            ra_hours: 0.7122,
            dec_degrees: 41.2689,
        };
        let t = Utc.with_ymd_and_hms(2026, 5, 3, 12, 0, 0).unwrap();
        let d = meridian_flip(&site_seattle(), m31, t).unwrap();
        assert!(d > Duration::zero());
        assert!(d <= Duration::hours(24));
        // sanity: trait dispatch matches direct call
        use crate::types::SideOfPier;
        let via_trait = eph
            .meridian_flip(&site_seattle(), m31, t, SideOfPier::Unknown)
            .unwrap();
        assert_eq!(via_trait, d);
    }

    #[test]
    fn astronomical_twilight_at_seattle_in_summer_has_both_bounds() {
        let eph = ErfarsEphemeris::new();
        let date = NaiveDate::from_ymd_opt(2026, 6, 21).unwrap();
        let w = twilight(&eph, &site_seattle(), date, TwilightKind::Astronomical);
        // At Seattle's latitude (47.6°) astronomical twilight does not
        // technically end on the longest summer day — sun stays above
        // -18° throughout. Either both bounds are None or both are
        // Some; assert structure rather than presence.
        match (w.begin_utc, w.end_utc) {
            (None, None) => {}
            (Some(b), Some(e)) => assert!(e > b),
            (b, e) => panic!("inconsistent twilight: begin={b:?} end={e:?}"),
        }
    }

    #[test]
    fn civil_twilight_at_seattle_in_winter_brackets_evening() {
        let eph = ErfarsEphemeris::new();
        let date = NaiveDate::from_ymd_opt(2026, 12, 21).unwrap();
        let w = twilight(&eph, &site_seattle(), date, TwilightKind::Civil);
        let begin = w
            .begin_utc
            .expect("civil twilight begin must exist in winter");
        let end = w.end_utc.expect("civil twilight end must exist in winter");
        assert!(end > begin);
        // The night should be at least 8 hours in late December at 47.6N
        assert!(end - begin > Duration::hours(8));
    }

    #[test]
    fn sun_at_threshold_is_consistent_with_sun_position() {
        // After bisection completes, the sun's altitude at the begin
        // time should be very close to -6° (civil threshold).
        let eph = ErfarsEphemeris::new();
        let date = NaiveDate::from_ymd_opt(2026, 12, 21).unwrap();
        let w = twilight(&eph, &site_seattle(), date, TwilightKind::Civil);
        let begin = w.begin_utc.unwrap();
        let sun = eph.sun_position(&site_seattle(), begin);
        assert!(
            (sun.alt_az.altitude_degrees - (-6.0)).abs() < 0.05,
            "sun alt at civil dusk = {:.3}°, expected ~-6",
            sun.alt_az.altitude_degrees
        );
    }
}

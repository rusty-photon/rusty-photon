//! Encoder-tick ↔ celestial-coordinate conversions.
//!
//! The mount's wire protocol speaks raw encoder ticks; ASCOM speaks RA/Dec
//! (hours and degrees). Bridging the two requires:
//!
//! * Counts-per-revolution (per axis, queried at handshake — see
//!   [`MountParameters`](crate::manager::MountParameters)).
//! * The sync offset (added on read, subtracted on write — set by
//!   `SyncToCoordinates`).
//! * Local apparent sidereal time (computed from host UTC + site
//!   longitude).
//! * Site latitude (for Az/Alt and side-of-pier derivation).
//!
//! These functions are pure — given the same parameters, they always return
//! the same answer. They are unit-tested directly without the transport
//! layer in scope.

use std::time::SystemTime;

use ascom_alpaca::api::telescope::PierSide;
use chrono::{DateTime, Datelike, Timelike, Utc};
use erfars::constants::ERFA_DPI;
use erfars::rotationtime::Gst06a;
use erfars::timescales::{Dtf2d, Taitt, Utctai};

use crate::config::FlipPolicy;
use crate::error::{Result, StarAdvError};
use crate::units::{sat_round_u32, Cpr, Dec, DecTicks, Lst, Ra, RaTicks};

/// Local apparent sidereal time in hours `[0, 24)` from the host's wall
/// clock and the configured site longitude (east-positive, ASCOM
/// convention).
///
/// Uses ERFA's `Gst06a` for Greenwich apparent sidereal time, then adds
/// the site longitude. Same approach as
/// `crates/rp-ephemeris/src/erfars_impl.rs::lst_hours`.
///
/// Returns [`StarAdvError::Timekeeping`] when ERFA refuses the host
/// UTC — in practice that means `eraCal2jd` (reached transitively
/// through `Dtf2d`) returning `-1` for a year below its calendar
/// floor (`IYMIN = -4799`), or above the analogous upper bound. The
/// leap-second-table boundary (years before 1960 or beyond `IYV + 5`)
/// returns an ERFA *warning* in `Utctai`, not an error, and is
/// silently accepted here. The coordinate-read hot path maps any Err
/// to an ASCOM error rather than letting the tokio task abort and
/// the Alpaca client see a connection reset.
pub fn local_sidereal_time_hours(utc: SystemTime, site_longitude_deg: f64) -> Result<Lst> {
    lst_for_datetime(utc.into(), site_longitude_deg)
}

/// `DateTime<Utc>`-typed seam underneath
/// [`local_sidereal_time_hours`]. Kept `pub(crate)` so unit tests can
/// drive ERFA-refusing dates directly without going through
/// `SystemTime` (which on Windows is backed by FILETIME and cannot
/// represent years below 1601, let alone below ERFA's `IYMIN = -4799`
/// floor).
pub(crate) fn lst_for_datetime(dt: DateTime<Utc>, site_longitude_deg: f64) -> Result<Lst> {
    let gast_hours = greenwich_apparent_sidereal_hours(dt)?;
    Ok(Lst::new(gast_hours + site_longitude_deg / 15.0))
}

fn greenwich_apparent_sidereal_hours(dt: DateTime<Utc>) -> Result<f64> {
    let year = dt.year();
    let month = dt.month().cast_signed();
    let day = dt.day().cast_signed();
    let hh = dt.hour().cast_signed();
    let mm = dt.minute().cast_signed();
    let seconds = f64::from(dt.second()) + f64::from(dt.nanosecond()) * 1e-9;

    let (utc1, utc2) = Dtf2d(true, year, month, day, hh, mm, seconds)
        .map_err(|code| {
            StarAdvError::Timekeeping(format!(
                "ERFA Dtf2d rejected UTC {year:04}-{month:02}-{day:02} \
                 {hh:02}:{mm:02}:{seconds:06.3} (code {code})"
            ))
        })?
        .0;
    let (tai1, tai2) = Utctai(utc1, utc2)
        .map_err(|code| {
            StarAdvError::Timekeeping(format!(
                "ERFA Utctai failed for UTC {year:04}-{month:02}-{day:02} \
                 (code {code})"
            ))
        })?
        .0;
    let (tt1, tt2) = Taitt(tai1, tai2);
    // ΔUT1 = 0; UT1 ≈ UTC for amateur tracking purposes.
    let gast_radians = Gst06a(utc1, utc2, tt1, tt2);
    Ok(gast_radians * 12.0 / ERFA_DPI)
}

/// Side-of-pier classification derived from the Dec-axis encoder
/// position, the Dec-axis CPR, and the site latitude.
///
/// Per the ASCOM spec, `pierEast` means "Through the Pole — the
/// cross-axis (Dec) has rotated 180° from its initial pier side";
/// `pierWest` is the "Normal pointing" state (cross-axis has not
/// rotated 180°). It's an *operational* (pre-flip vs post-flip)
/// classification of the Dec axis, not a geometric "OTA east of
/// pier" classification of the saddle's mechanical position.
///
/// For a Northern Hemisphere observer, the Dec encoder stays within
/// ±90° (= `±cpr_dec/4` ticks) of home for any target reached
/// without a meridian flip → `PierSide::West`. Once the mount
/// rotates the Dec axis past the celestial pole (encoder magnitude
/// past 90°) the cross-axis has rotated through 180° →
/// `PierSide::East`. Southern hemisphere inverts the convention.
///
/// This is INDI eqmod's canonical GEM convention (see
/// `eqmodbase.cpp::EncodersToRADec` — `DECurrent > 90° && <= 270°`
/// → `PIER_EAST`, equivalent to the magnitude-past-90° check applied
/// to the signed encoder reading our driver carries).
///
/// Caveat: this differs from the AP "Park Positions Defined" naming,
/// which labels poses by saddle position rather than dec-axis state.
/// AP Park 1 N (saddle west, dec past pole) reports `pierEast`
/// here; AP Park 5 N (saddle east, dec normal) reports `pierWest`.
/// The encoder values for each park pose are still mechanically
/// correct — only the operational-vs-geometric label disagrees.
#[must_use]
pub fn side_of_pier(dec_ticks: DecTicks, cpr_dec: Cpr, site_latitude_deg: f64) -> PierSide {
    if cpr_dec.get() == 0 {
        return PierSide::Unknown;
    }
    let quarter = i64::from(cpr_dec.get() / 4);
    // Fold the raw counter into the canonical band before classifying:
    // raw can sit outside `[-cpr/2, +cpr/2)` after through-wrap flip
    // slews, and the half-classification only makes sense on the folded
    // position. Without folding, a raw in `(3·cpr/4, 5·cpr/4)` whose
    // folded value lies in `(-cpr/4, +cpr/4)` would be misread as
    // post-flip and the East/West label would invert.
    let folded = dec_ticks.fold_to_canonical_band(cpr_dec).value();
    // Past-the-pole detection: |Dec encoder| > 90° means the mount
    // has rotated the Dec axis beyond either celestial pole — the
    // post-meridian-flip / cross-axis-rotated-180° state. The
    // boundary at exactly ±90° is *not* East: the mount can sit at
    // the pole via normal-pointing slews without a flip.
    let east_in_north = i64::from(folded).abs() > quarter;
    let northern = site_latitude_deg >= 0.0;
    let east = if northern {
        east_in_north
    } else {
        !east_in_north
    };
    if east {
        PierSide::East
    } else {
        PierSide::West
    }
}

/// Compute the target's RA/Dec encoder pair for the "normal"
/// (pre-flip) pointing state.
///
/// The RA encoder is mapped from the mechanical hour-angle
/// `mech_HA = LST − target_RA` (signed, folded to `[−12, +12)`) and
/// the Dec encoder is the celestial declination — the existing
/// behaviour for every slew before Phase 6, extracted into a helper
/// so [`target_encoder_flipped`] can share the structure.
#[must_use]
pub fn target_encoder_normal(
    ra: Ra,
    dec: Dec,
    lst: Lst,
    cpr_ra: Cpr,
    cpr_dec: Cpr,
) -> (RaTicks, DecTicks) {
    let mech_ha = lst.hour_angle_of(ra).to_mech();
    let ra_ticks = mech_ha.to_ticks(cpr_ra);
    let dec_ticks = dec.to_mech().to_ticks(cpr_dec);
    (ra_ticks, dec_ticks)
}

/// Compute the target's RA/Dec encoder pair for the "flipped"
/// (post-meridian-flip) pointing state.
///
/// `mech_HA_flipped = mech_HA_normal + 12 h` (folded to `[−12, +12)`)
/// puts the RA encoder at the mirror position across the encoder
/// wrap at `±12 h`. `dec_encoder_flipped = sign(dec) · (180° − |dec|)`
/// puts the Dec encoder past the celestial pole on the same
/// hemispheric side — the OTA rotates through the pole to land on the
/// other end of the Dec axis, keeping the OTA on the same celestial
/// target while the counterweight crosses to the opposite side of the
/// pier. See the design doc's
/// [§"Meridian flip"](../../../docs/services/star-adventurer-gti.md#meridian-flip).
///
/// The `dec = 0` case is degenerate: `sign(0.0).signum()` is `+1.0`,
/// so the flipped encoder lands at exactly `+180°` (or equivalently
/// `−180°` after fold). Both encoder values reduce to the same
/// physical mechanical position at the encoder wrap; downstream
/// callers don't distinguish between them.
#[must_use]
pub fn target_encoder_flipped(
    ra: Ra,
    dec: Dec,
    lst: Lst,
    cpr_ra: Cpr,
    cpr_dec: Cpr,
) -> (RaTicks, DecTicks) {
    let mech_ha_flipped = lst.hour_angle_of(ra).to_mech().flipped();
    let ra_ticks = mech_ha_flipped.to_ticks(cpr_ra);
    let dec_ticks = dec.to_mech_flipped().to_ticks(cpr_dec);
    (ra_ticks, dec_ticks)
}

/// Compute the OTA's celestial pointing `(RA hours, Dec degrees)`
/// from the encoder snapshot + LST + latitude.
///
/// Inverse of [`target_encoder_normal`] / [`target_encoder_flipped`].
/// Detects post-flip state from the Dec encoder past the celestial
/// pole (the same convention [`side_of_pier`] uses) and applies the
/// corresponding celestial mapping:
///
/// - Pre-flip: `RA = (LST − mech_HA) mod 24`, `Dec = dec_encoder`.
/// - Post-flip: `RA = (LST − mech_HA + 12) mod 24`, `Dec = sign(dec_enc) · (180° − |dec_enc|)`.
///
/// Without the post-flip correction every read path
/// (`right_ascension`, `declination`, `altitude`, `azimuth`, and the
/// slew watcher's pickup-loop residual check) would report the
/// encoder-direction RA/Dec rather than the OTA's celestial
/// pointing, and the pickup loop would interpret a successful flip
/// as a 12-hour RA residual and try to undo it.
#[must_use]
pub fn encoder_to_celestial(
    ra_ticks: RaTicks,
    dec_ticks: DecTicks,
    lst: Lst,
    cpr_ra: Cpr,
    cpr_dec: Cpr,
    site_latitude_deg: f64,
) -> (Ra, Dec) {
    let mech_ha = ra_ticks.to_mech_ha(cpr_ra);
    let dec_enc = dec_ticks.to_mech_dec(cpr_dec);
    let pier = side_of_pier(dec_ticks, cpr_dec, site_latitude_deg);
    let pre_flip_side = if site_latitude_deg >= 0.0 {
        PierSide::West
    } else {
        PierSide::East
    };
    let is_flipped = pier != pre_flip_side && pier != PierSide::Unknown;
    if is_flipped {
        let ra = mech_ha.to_ra_flipped(lst);
        // The degenerate `dec_enc == 0` post-flip case (dec encoder at the
        // wrap) is unreachable when `is_flipped == true` because
        // `side_of_pier`'s `|dec_ticks| > cpr/4` check is strict.
        let dec = dec_enc.to_dec_flipped();
        (ra, dec)
    } else {
        let ra = mech_ha.to_ra(lst);
        (ra, dec_enc.to_dec())
    }
}

/// Return the ASCOM-opposite pier side. `Unknown` maps to `Unknown`
/// — the driver does not invent a side it has no information about.
#[must_use]
pub const fn opposite_pier_side(side: PierSide) -> PierSide {
    match side {
        PierSide::West => PierSide::East,
        PierSide::East => PierSide::West,
        PierSide::Unknown => PierSide::Unknown,
    }
}

/// Choose the target pier side for a slew (or for the
/// `DestinationSideOfPier` prediction).
///
/// Decision tree (mirrors the design doc's
/// [§"Pier-side decision tree"](../../../docs/services/star-adventurer-gti.md#pier-side-decision-tree)):
///
/// 1. If `policy.enabled == false`, return `current` unchanged.
/// 2. Compute the target's celestial-HA and the resulting `mech_HA` on
///    each pier side (`mech_HA_normal = HA`; `mech_HA_flipped = HA + 12`
///    folded).
/// 3. If the *current* side can reach the target without entering the
///    counterweight binding zone, stay on the current side. For the
///    pre-flip side that's `mech_HA_normal ∉ binding_zone`; for the
///    post-flip side that's `|target_HA| ≤ flip_range_hours` (the
///    operational rule that keeps the mount on flipped only briefly
///    past the meridian — there's no mechanical reason to leave the
///    flipped side at e.g. `mech_HA_flipped = 0` ≈ anti-meridian, but
///    the operational convention is to flip back).
/// 4. Otherwise return [`opposite_pier_side`]`(current)`.
///
/// When `current` is [`PierSide::Unknown`] the helper returns
/// `Unknown` regardless of policy — the driver has no encoder
/// classification to anchor a flip decision on. This mirrors how
/// [`side_of_pier`] degrades when `cpr_dec == 0`.
#[must_use]
pub fn select_pier_side_for_target(
    target_ra: Ra,
    lst: Lst,
    current: PierSide,
    policy: &FlipPolicy,
    binding_zone_hours: (f64, f64),
    site_latitude_deg: f64,
) -> PierSide {
    if !policy.enabled {
        return current;
    }
    if current == PierSide::Unknown {
        return PierSide::Unknown;
    }
    let target_hour_angle = lst.hour_angle_of(target_ra).value();
    let northern = site_latitude_deg >= 0.0;
    let pre_flip_side = if northern {
        PierSide::West
    } else {
        PierSide::East
    };
    let current_covers = if current == pre_flip_side {
        // Natural side: mech_HA = celestial HA. "Covers" means not in
        // binding zone — including the safe wrap region near ±12. The
        // zone is the **open** interval `(zone_min, zone_max)` with
        // `zone_min >= zone_max` meaning disabled, matching
        // `check_within_safe_envelope`, `canonical_path_crosses_binding_zone`,
        // and `tracking_guard_breached`. A target landing exactly on a
        // boundary is therefore *not* in the zone — the envelope check
        // would permit it on this side, so the selector must not force a
        // spurious flip.
        let (zone_min, zone_max) = binding_zone_hours;
        !(zone_min < zone_max && target_hour_angle > zone_min && target_hour_angle < zone_max)
    } else {
        // Post-flip side: operational rule, stay on flipped only near
        // meridian. The binding zone for flipped-side mech_HA is
        // separately enforced by `check_within_safe_envelope`.
        target_hour_angle.abs() <= policy.flip_range_hours.value()
    };
    if current_covers {
        current
    } else {
        opposite_pier_side(current)
    }
}

/// Compute the RA-axis encoder target for a pickup-loop re-slew,
/// pre-compensated for the LST drift expected to occur before the
/// next residual check.
///
/// Without pre-compensation, each pickup iteration computes
/// `mech_HA = LST(now) - target_RA` and aims the encoder there. But
/// the slew takes ~one iteration to settle, by which time LST has
/// advanced ~`projection × 15.04″/sec`. The next iteration sees the
/// encoder is short of where it should be (because `target_RA` has the
/// same value but `mech_HA` needed = `LST_now+iter` - `target_RA` is now
/// larger). Pickup is locked into chasing a moving target and the
/// residual floor matches the per-iteration LST drift.
///
/// Pre-compensation: aim where the encoder *will need to be* at the
/// next check, i.e. compute `mech_HA = LST(now + projection) -
/// target_RA`. By the time the slew settles and the next iteration
/// re-checks, LST has advanced into the projection and the encoder
/// is exactly on target.
///
/// `projection` is the watcher's expected per-iteration duration —
/// see the slew-completion watcher in `crate::mount_device::watchers`.
/// On a per-`polling_interval` cadence the empirical observation is
/// ~`polling_interval × 2` (one watcher sleep + one slew-settle
/// + wire round-trips).
#[must_use]
pub fn pickup_target_ra_ticks(
    target_ra: Ra,
    current_lst: Lst,
    projection: std::time::Duration,
    cpr_ra: Cpr,
) -> RaTicks {
    let lst_at_next_check = Lst::new(current_lst.value() + projection.as_secs_f64() / 3600.0);
    lst_at_next_check
        .hour_angle_of(target_ra)
        .to_mech()
        .to_ticks(cpr_ra)
}

/// Convert RA / Dec → topocentric (altitude, azimuth) given the site
/// latitude and the current LST.
///
/// Returns `(altitude_degrees, azimuth_degrees)`. Azimuth is measured
/// clockwise from north in the range `[0, 360)`. Refraction is **not**
/// applied — the design doc keeps the driver refraction-free
/// (`DoesRefraction = false`).
#[must_use]
pub fn ra_dec_to_alt_az(ra: Ra, dec: Dec, site_latitude_deg: f64, lst: Lst) -> (f64, f64) {
    let ha_rad = ((lst.value() - ra.value()) * 15.0).to_radians();
    let dec_rad = dec.value().to_radians();
    let lat_rad = site_latitude_deg.to_radians();
    let sin_alt = dec_rad.sin() * lat_rad.sin() + dec_rad.cos() * lat_rad.cos() * ha_rad.cos();
    let alt_rad = sin_alt.clamp(-1.0, 1.0).asin();
    let cos_alt = alt_rad.cos();
    // Avoid divide-by-zero at the zenith. Az is undefined there; return 0.
    let az_rad = if cos_alt.abs() < 1e-12 {
        0.0
    } else {
        let sin_az = -dec_rad.cos() * ha_rad.sin() / cos_alt;
        let cos_az = (dec_rad.sin() - sin_alt * lat_rad.sin()) / (cos_alt * lat_rad.cos());
        sin_az.atan2(cos_az)
    };
    let alt_deg = alt_rad.to_degrees();
    let az_deg = az_rad.to_degrees().rem_euclid(360.0);
    (alt_deg, az_deg)
}

/// Sidereal step-period (T1 preset) for the `:I` command, in
/// timer-counter units.
///
/// The mount's step rate is `tmr_freq / period` steps/sec; one full
/// revolution is `cpr` steps; one sidereal day is `86164.0905` seconds.
/// Solving for `period` so the rotation rate matches the sidereal rate:
///
/// `period = tmr_freq * 86164.0905 / cpr`
///
/// For the `GTi` defaults (`tmr_freq = 0xF42400 = 16_000_000`,
/// `cpr = 0x375F00 = 3_628_800`), this gives roughly `379,887`.
#[must_use]
pub fn sidereal_step_period(tmr_freq: u32, cpr_ra: Cpr) -> u32 {
    if cpr_ra.get() == 0 {
        return 0;
    }
    let sidereal_seconds = 86164.0905_f64;
    sat_round_u32(f64::from(tmr_freq) * sidereal_seconds / f64::from(cpr_ra.get()))
}

/// Sidereal rate in degrees per second.
///
/// `360° / 86164.0905 s ≈ 4.17807e-3 deg/sec ≈ 15.04108″/sec`. ASCOM
/// `GuideRateRightAscension` / `GuideRateDeclination` are expressed in
/// these units; internally the driver uses a fraction of sidereal in
/// `(0, 1)` for the rate-shift math.
pub const SIDEREAL_DEG_PER_SEC: f64 = 360.0 / 86164.0905;

/// Rate-shifted step period for a `PulseGuide` burst, in timer-counter
/// units (the same units `:I` takes).
///
/// `rate_factor` is the target rate as a multiple of sidereal:
///   - East  → `1.0 - ra_fraction` (RA tracking slowed)
///   - West  → `1.0 + ra_fraction` (RA tracking sped up)
///   - North → `dec_fraction`      (Dec spun from zero)
///   - South → `dec_fraction`
///
/// Step period scales inversely with rate
/// (`period = sidereal_period / rate_factor`).
///
/// The caller is responsible for keeping `rate_factor` strictly
/// positive — `pulse_guide`'s upstream validation rejects a guide-rate
/// fraction outside `(0, 1)`, so the East formula `1 - fraction`
/// stays in `(0, 1)` and never zeroes the divisor.
#[must_use]
pub fn pulse_guide_step_period(sidereal_period: u32, rate_factor: f64) -> u32 {
    debug_assert!(
        rate_factor > 0.0,
        "rate_factor must be positive (got {rate_factor})"
    );
    sat_round_u32(f64::from(sidereal_period) / rate_factor)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    const GTI_CPR: u32 = 0x0037_5F00;

    fn cpr() -> Cpr {
        Cpr::new(GTI_CPR)
    }

    #[test]
    fn lst_changes_with_longitude() {
        // Two LSTs at the same UTC, 90° apart in longitude, must be 6
        // hours apart.
        let utc = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let lst_0 = local_sidereal_time_hours(utc, 0.0).unwrap().value();
        let lst_e = local_sidereal_time_hours(utc, 90.0).unwrap().value();
        let diff = (lst_e - lst_0).rem_euclid(24.0);
        assert!((diff - 6.0).abs() < 1e-6, "LST(90E) - LST(0) = {diff}h");
    }

    #[test]
    fn lst_is_stable_across_calls() {
        // Same input → same output.
        let utc = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert_eq!(
            local_sidereal_time_hours(utc, -122.4).unwrap(),
            local_sidereal_time_hours(utc, -122.4).unwrap()
        );
    }

    #[test]
    fn lst_returns_timekeeping_error_for_year_below_erfa_floor() {
        // ERFA's `eraCal2jd` (reached transitively via `Dtf2d`)
        // rejects years below `IYMIN = -4799` with status `-1`,
        // which `Dtf2d` re-maps to its own `-1` ("bad year"). This
        // is the recoverable error path that replaced the original
        // panic.
        //
        // We drive the chrono-typed seam directly so the test is
        // platform-portable — Windows `SystemTime` is backed by
        // FILETIME and cannot represent years below 1601, but
        // `chrono::NaiveDate` spans `-262_143` through `262_143`
        // (well past ERFA's floor) on every platform.
        use chrono::NaiveDate;
        let dt = NaiveDate::from_ymd_opt(-5000, 1, 1)
            .expect("year -5000 inside chrono's NaiveDate range")
            .and_hms_opt(0, 0, 0)
            .expect("midnight is a valid time of day")
            .and_utc();
        let err = lst_for_datetime(dt, 0.0).unwrap_err();
        assert!(
            matches!(err, StarAdvError::Timekeeping(_)),
            "expected Timekeeping error, got {err:?}"
        );
    }

    #[test]
    fn side_of_pier_north_equator_is_west() {
        // Dec encoder at home (0 ticks ≈ celestial equator on the
        // meridian). Mount is in normal-pointing state → pierWest.
        assert_eq!(side_of_pier(DecTicks::new(0), cpr(), 47.6), PierSide::West);
    }

    #[test]
    fn side_of_pier_north_within_envelope_is_west() {
        // Any encoder magnitude up to and including ±cpr/4 (= ±90°)
        // is reachable without a meridian flip. ConformU's SOPPierTest
        // exercises these cases; all four must read pierWest.
        let quarter = (GTI_CPR / 4).cast_signed();
        assert_eq!(
            side_of_pier(DecTicks::new(quarter / 3), cpr(), 47.6),
            PierSide::West
        );
        assert_eq!(
            side_of_pier(DecTicks::new(-quarter / 3), cpr(), 47.6),
            PierSide::West
        );
        // Boundary cases at exactly ±90°: the mount can reach either
        // celestial pole via normal pointing, so the boundary is
        // included in `West` (matches INDI eqmod's `> 90°` strict
        // check).
        assert_eq!(
            side_of_pier(DecTicks::new(quarter), cpr(), 47.6),
            PierSide::West
        );
        assert_eq!(
            side_of_pier(DecTicks::new(-quarter), cpr(), 47.6),
            PierSide::West
        );
    }

    #[test]
    fn side_of_pier_north_past_pole_is_east() {
        // Dec encoder magnitude past 90° means the mount has rotated
        // the Dec axis beyond the celestial pole — the post-flip /
        // counterweight-up state, which ASCOM names pierEast.
        let quarter = (GTI_CPR / 4).cast_signed();
        assert_eq!(
            side_of_pier(DecTicks::new(quarter + 1), cpr(), 47.6),
            PierSide::East
        );
        assert_eq!(
            side_of_pier(DecTicks::new(-(quarter + 1)), cpr(), 47.6),
            PierSide::East
        );
        // Mid-flip and "full flip to equator on the opposite side".
        let half = (GTI_CPR / 2).cast_signed();
        assert_eq!(
            side_of_pier(DecTicks::new(quarter + half / 4), cpr(), 47.6),
            PierSide::East
        );
        assert_eq!(
            side_of_pier(DecTicks::new(half), cpr(), 47.6),
            PierSide::East
        );
    }

    #[test]
    fn side_of_pier_southern_hemisphere_inverts() {
        // Mirror of the northern split.
        let quarter = (GTI_CPR / 4).cast_signed();
        assert_eq!(side_of_pier(DecTicks::new(0), cpr(), -33.9), PierSide::East);
        assert_eq!(
            side_of_pier(DecTicks::new(quarter / 3), cpr(), -33.9),
            PierSide::East
        );
        assert_eq!(
            side_of_pier(DecTicks::new(quarter), cpr(), -33.9),
            PierSide::East
        );
        assert_eq!(
            side_of_pier(DecTicks::new(quarter + 1), cpr(), -33.9),
            PierSide::West
        );
        assert_eq!(
            side_of_pier(DecTicks::new(-(quarter + 1)), cpr(), -33.9),
            PierSide::West
        );
    }

    #[test]
    fn side_of_pier_returns_unknown_when_cpr_is_zero() {
        // Degenerate case — would mean the parameter cache was never
        // populated. The accessor short-circuits on
        // `NOT_CONNECTED` before reaching the helper in practice, but
        // the helper still has to handle this defensively to stay a
        // total function.
        assert_eq!(
            side_of_pier(DecTicks::new(0), Cpr::new(0), 47.6),
            PierSide::Unknown
        );
    }

    #[test]
    fn side_of_pier_folds_raw_outside_canonical_band() {
        // `dec_ticks` is the raw encoder counter — it can drift outside
        // `[-cpr/2, +cpr/2)` after through-wrap flip slews, manual `:E`
        // writes, or power-up encoder noise. The East/West label must
        // reflect the *physical* dec-axis position (folded), not the raw
        // counter value. Raw in `(3·cpr/4, 5·cpr/4)` folds to within
        // `(-cpr/4, +cpr/4)` — i.e. pre-flip → pierWest for a Northern
        // observer.
        let cpr_i = GTI_CPR.cast_signed();
        let raw_positive_zone = cpr_i * 7 / 8; // 7·cpr/8, folds to -cpr/8
        assert!(raw_positive_zone.abs() > cpr_i / 4);
        let folded = DecTicks::new(raw_positive_zone)
            .fold_to_canonical_band(cpr())
            .value();
        assert!(folded.abs() <= cpr_i / 4, "must fold into pre-flip half");
        assert_eq!(
            side_of_pier(DecTicks::new(raw_positive_zone), cpr(), 47.6),
            PierSide::West,
            "raw in positive disagreement zone must classify by folded position"
        );
        // Mirror on the negative side.
        let raw_negative_zone = -cpr_i * 7 / 8;
        assert_eq!(
            side_of_pier(DecTicks::new(raw_negative_zone), cpr(), 47.6),
            PierSide::West,
            "raw in negative disagreement zone must classify by folded position"
        );
        // And the southern hemisphere mirror.
        assert_eq!(
            side_of_pier(DecTicks::new(raw_positive_zone), cpr(), -33.9),
            PierSide::East,
            "south: raw in disagreement zone must classify by folded position"
        );
    }

    #[test]
    fn alt_az_for_zenith_at_equator() {
        // At the equator, the celestial equator passes through the zenith
        // when the LST equals the target's RA.
        let (alt, _az) = ra_dec_to_alt_az(Ra::new(12.0), Dec::new(0.0), 0.0, Lst::new(12.0));
        assert!((alt - 90.0).abs() < 1e-6, "got alt={alt}");
    }

    #[test]
    fn alt_az_for_celestial_pole_at_north_observer() {
        // From a northern observer, the NCP sits at altitude = latitude.
        // (Matches the standard astronomy textbook result.)
        let (alt, _az) = ra_dec_to_alt_az(Ra::new(0.0), Dec::new(90.0), 47.6, Lst::new(12.0));
        assert!((alt - 47.6).abs() < 1e-6, "got alt={alt}");
    }

    #[test]
    fn altitude_matches_the_altitude_floor_worked_examples() {
        // The worked-example table backing the altitude-floor gate
        // (plan `star-adventurer-gti-meridian-flip.md` §3.1, LAT 45°N)
        // plus hemisphere/latitude extremes. LST is pinned to 12 h and
        // `RA = LST − HA`, so the HA column is exact.
        let alt = |ha_hours: f64, dec: f64, lat: f64| {
            ra_dec_to_alt_az(Ra::new(12.0 - ha_hours), Dec::new(dec), lat, Lst::new(12.0)).0
        };
        // Meridian target 5° above the south horizon.
        assert!((alt(0.0, -40.0, 45.0) - 5.0).abs() < 1e-6);
        // East horizon at the celestial equator: alt = 0 (edge).
        assert!(alt(-6.0, 0.0, 45.0).abs() < 1e-6);
        // Slightly east of meridian, still low above the horizon
        // (the plan's table rounds this row to ≈ 5°; exact is 5.33°).
        assert!((alt(-0.5, -39.4, 45.0) - 5.33).abs() < 0.05);
        // Low west, mid-Dec: below the horizon.
        assert!((alt(-3.0, -40.0, 45.0) - (-4.1)).abs() < 0.05);
        // Low west, higher Dec: above the horizon.
        assert!((alt(-3.0, -30.0, 45.0) - 4.56).abs() < 0.05);
        // NCP sits at alt = latitude regardless of HA.
        assert!((alt(1.234, 90.0, 45.0) - 45.0).abs() < 1e-6);
        // Equator observer: equatorial target at HA 0 is at zenith.
        assert!((alt(0.0, 0.0, 0.0) - 90.0).abs() < 1e-6);
        // North-pole observer: alt = dec at any HA.
        assert!((alt(4.0, 45.0, 90.0) - 45.0).abs() < 1e-6);
        // Southern hemisphere: dec = lat is the zenith.
        assert!((alt(0.0, -45.0, -45.0) - 90.0).abs() < 1e-6);
    }

    #[test]
    fn sidereal_step_period_for_gti_defaults() {
        // tmr_freq = 16M, cpr = 3,628,800 → period ≈ 379,887.
        let p = sidereal_step_period(0x00F4_2400, cpr());
        assert!((379_000..=380_000).contains(&p), "expected ~380K, got {p}");
    }

    #[test]
    fn pickup_target_zero_projection_matches_unprojected_math() {
        // With zero projection, pickup target must equal the same
        // mech_ha → ticks math the slew-issue path uses. This is the
        // backwards-compat case: pre-compensation off ⇒ identical
        // wire behaviour to before the change.
        let target_ra = Ra::new(6.0);
        let lst = Lst::new(12.0);
        let unprojected = lst.hour_angle_of(target_ra).to_mech().to_ticks(cpr());
        let projected_zero =
            pickup_target_ra_ticks(target_ra, lst, std::time::Duration::ZERO, cpr());
        assert_eq!(projected_zero, unprojected);
    }

    #[test]
    fn pickup_target_400ms_projection_advances_by_sidereal_drift() {
        // 400 ms of LST drift at sidereal rate = 400 ms × 15.04″/s ≈ 6.016″.
        // Converted to encoder ticks at GTi CPR: 6.016″ / 3600 = 0.001671°
        // of RA = (0.001671° × 4 min/°) hours of mech_HA. At 15°/hour, 1
        // sidereal second of mech_HA = (1/3600) hours, and 400 ms = 0.4
        // sidereal seconds → 0.4/3600 hours of mech_HA.
        // Ticks per hour of mech_HA = cpr/24 = 151,200. So 400 ms ≈
        // 0.4/3600 × 151,200 ≈ 16.8 ticks.
        let target_ra = Ra::new(6.0);
        let lst = Lst::new(12.0);
        let no_proj =
            pickup_target_ra_ticks(target_ra, lst, std::time::Duration::ZERO, cpr()).value();
        let projected =
            pickup_target_ra_ticks(target_ra, lst, std::time::Duration::from_millis(400), cpr())
                .value();
        let delta = projected - no_proj;
        // Expected: ~16-17 ticks. Tolerance +/-1 for round() boundary.
        assert!(
            (15..=18).contains(&delta),
            "expected delta of ~16-17 ticks for 400ms LST projection, got {delta}"
        );
    }

    #[test]
    fn pickup_target_projection_scales_linearly_with_duration() {
        // Doubling the projection should double the tick delta (within
        // round-to-int noise).
        let target_ra = Ra::new(6.0);
        let lst = Lst::new(12.0);
        let zero = pickup_target_ra_ticks(target_ra, lst, std::time::Duration::ZERO, cpr()).value();
        let d1 =
            pickup_target_ra_ticks(target_ra, lst, std::time::Duration::from_millis(200), cpr())
                .value()
                - zero;
        let d2 =
            pickup_target_ra_ticks(target_ra, lst, std::time::Duration::from_millis(400), cpr())
                .value()
                - zero;
        assert!(
            (d2 - 2 * d1).abs() <= 1,
            "expected ~2× scaling: 200ms→{d1}, 400ms→{d2}"
        );
    }

    #[test]
    fn pulse_guide_step_period_identity_at_unit_rate_factor() {
        // Rate factor = 1.0 reproduces the sidereal period exactly
        // (modulo rounding to integer).
        let p_sid = sidereal_step_period(0x00F4_2400, cpr());
        assert_eq!(pulse_guide_step_period(p_sid, 1.0), p_sid);
    }

    #[test]
    fn pulse_guide_step_period_halves_rate_doubles_period() {
        // Rate factor = 0.5 (Dec North/South at fraction = 0.5, or East
        // at fraction = 0.5) doubles the step period.
        let p_sid = sidereal_step_period(0x00F4_2400, cpr());
        let shifted = pulse_guide_step_period(p_sid, 0.5);
        assert_eq!(shifted, 2 * p_sid);
    }

    #[test]
    fn pulse_guide_step_period_west_at_fraction_half_uses_one_and_a_half_rate() {
        // West at fraction = 0.5 → rate_factor = 1.5 → period = P_sid / 1.5.
        let p_sid = sidereal_step_period(0x00F4_2400, cpr());
        let shifted = pulse_guide_step_period(p_sid, 1.5);
        let expected = (f64::from(p_sid) / 1.5).round() as u32;
        assert_eq!(shifted, expected);
        // Sanity: must be smaller than sidereal (faster rate ⇒ shorter
        // period).
        assert!(shifted < p_sid);
    }

    #[test]
    fn pulse_guide_step_period_small_fraction_grows_period_proportionally() {
        // fraction = 0.1 on Dec → rate_factor = 0.1 → period = 10 × sidereal.
        let p_sid = sidereal_step_period(0x00F4_2400, cpr());
        let shifted = pulse_guide_step_period(p_sid, 0.1);
        assert_eq!(shifted, 10 * p_sid);
    }

    // ---------- Phase 6: target_encoder_{normal,flipped} ----------

    #[test]
    fn target_encoder_normal_matches_existing_ra_dec_to_ticks_pipeline() {
        // The "normal" helper is the existing slew-issue pipeline,
        // extracted. Targets at LST=12h, RA=12h → mech_HA=0 (meridian);
        // any dec → encoder reflects that dec.
        let (ra_ticks, dec_ticks) =
            target_encoder_normal(Ra::new(12.0), Dec::new(30.0), Lst::new(12.0), cpr(), cpr());
        assert_eq!(ra_ticks.value(), 0);
        // Dec encoder at 30° / 360° × CPR.
        let expected_dec = (30.0 * f64::from(GTI_CPR) / 360.0).round() as i32;
        assert_eq!(dec_ticks.value(), expected_dec);
    }

    #[test]
    fn target_encoder_flipped_at_lat45_zenith_east_matches_plan_example() {
        // Plan §2.0 worked example, LAT 45°N: target HA = −0.5,
        // dec = +45°. Pre-flip mech_HA = −0.5, dec_enc = +45°. Post-flip
        // mech_HA = +11.5, dec_enc = +135°.
        let lst = 5.0; // arbitrary
        let target_ra = lst + 0.5; // mech_HA = lst − ra = −0.5
        let target_dec = 45.0;
        let (ra_ticks, dec_ticks) = target_encoder_flipped(
            Ra::new(target_ra),
            Dec::new(target_dec),
            Lst::new(lst),
            cpr(),
            cpr(),
        );
        let mech_ha_flipped = ra_ticks.to_mech_ha(cpr()).value();
        let dec_enc_flipped = dec_ticks.to_mech_dec(cpr()).value();
        assert!(
            (mech_ha_flipped - 11.5).abs() < 1e-6,
            "expected mech_HA_flipped ≈ +11.5, got {mech_ha_flipped}"
        );
        assert!(
            (dec_enc_flipped - 135.0).abs() < 1e-6,
            "expected dec_enc_flipped ≈ +135°, got {dec_enc_flipped}"
        );
    }

    #[test]
    fn target_encoder_flipped_wraps_through_negative_for_positive_target_ha() {
        // target_HA = +0.5 (just west of meridian). Pre-flip mech_HA =
        // +0.5; post-flip = +0.5 + 12 = +12.5 → folds to −11.5.
        let lst = 5.0;
        let target_ra = lst - 0.5; // mech_HA = +0.5
        let (ra_ticks, _dec) = target_encoder_flipped(
            Ra::new(target_ra),
            Dec::new(30.0),
            Lst::new(lst),
            cpr(),
            cpr(),
        );
        let mech_ha_flipped = ra_ticks.to_mech_ha(cpr()).value();
        assert!(
            (mech_ha_flipped + 11.5).abs() < 1e-6,
            "expected mech_HA_flipped ≈ −11.5, got {mech_ha_flipped}"
        );
    }

    #[test]
    fn target_encoder_flipped_at_meridian_lands_at_encoder_wrap() {
        // target_HA = 0 (exact meridian). Post-flip mech_HA = 0 + 12 = 12
        // → folds to −12 (per fold_to_signed's `folded >= half → −period`
        // branch).
        let lst = 5.0;
        let target_ra = lst; // mech_HA = 0
        let (ra_ticks, _) = target_encoder_flipped(
            Ra::new(target_ra),
            Dec::new(30.0),
            Lst::new(lst),
            cpr(),
            cpr(),
        );
        let mech_ha_flipped = ra_ticks.to_mech_ha(cpr()).value();
        assert!(
            (mech_ha_flipped + 12.0).abs() < 1e-6 || (mech_ha_flipped - 12.0).abs() < 1e-6,
            "expected mech_HA_flipped at the ±12 wrap, got {mech_ha_flipped}"
        );
    }

    #[test]
    fn target_encoder_flipped_dec_negative_target_flips_through_south_pole() {
        // dec = −45° → flipped dec_enc = −(180 − 45) = −135°.
        let (_, dec_ticks) =
            target_encoder_flipped(Ra::new(6.0), Dec::new(-45.0), Lst::new(6.0), cpr(), cpr());
        let dec_enc = dec_ticks.to_mech_dec(cpr()).value();
        assert!(
            (dec_enc + 135.0).abs() < 1e-6,
            "expected dec_enc ≈ −135°, got {dec_enc}"
        );
    }

    #[test]
    fn target_encoder_flipped_dec_at_pole_stays_at_pole() {
        // dec = +90° (celestial pole). Flipped encoder = sign(+90) ·
        // (180 − 90) = +90. The pole is the same physical position
        // whether you're flipped or not — only the OTA's rotation about
        // its optical axis differs.
        let (_, dec_ticks) =
            target_encoder_flipped(Ra::new(6.0), Dec::new(90.0), Lst::new(6.0), cpr(), cpr());
        let dec_enc = dec_ticks.to_mech_dec(cpr()).value();
        assert!(
            (dec_enc - 90.0).abs() < 1e-6,
            "expected dec_enc ≈ +90°, got {dec_enc}"
        );
    }

    // ---------- Phase 6: select_pier_side_for_target ----------

    fn flip_disabled() -> FlipPolicy {
        FlipPolicy {
            enabled: false,
            flip_range_hours: crate::config::FlipRangeHours::new(0.5),
            ..Default::default()
        }
    }
    fn flip_enabled() -> FlipPolicy {
        FlipPolicy {
            enabled: true,
            flip_range_hours: crate::config::FlipRangeHours::new(0.5),
            ..Default::default()
        }
    }
    const BINDING_ZONE: (f64, f64) = (6.95, 11.05);
    const LAT_NORTH: f64 = 45.0;
    const LAT_SOUTH: f64 = -33.0;

    #[test]
    fn select_pier_side_when_policy_disabled_returns_current() {
        let policy = flip_disabled();
        let lst = 12.0;
        // Even with target way outside the pre-flip envelope, !enabled
        // means "leave the side alone".
        for current in [PierSide::West, PierSide::East, PierSide::Unknown] {
            let chosen = select_pier_side_for_target(
                Ra::new(0.0),
                Lst::new(lst),
                current,
                &policy,
                BINDING_ZONE,
                LAT_NORTH,
            );
            assert_eq!(chosen, current, "current={current:?}");
        }
    }

    #[test]
    fn select_pier_side_returns_unknown_when_current_is_unknown() {
        // No encoder info means no flip decision — even with the policy
        // enabled, return Unknown.
        let policy = flip_enabled();
        let chosen = select_pier_side_for_target(
            Ra::new(0.0),
            Lst::new(12.0),
            PierSide::Unknown,
            &policy,
            BINDING_ZONE,
            LAT_NORTH,
        );
        assert_eq!(chosen, PierSide::Unknown);
    }

    #[test]
    fn select_pier_side_north_pierwest_stays_for_inside_pre_flip_envelope() {
        // Northern hemisphere, currently on the "normal" (pre-flip) side
        // = pierWest. Target HA = −3 (well inside [−6.95, +6.95]). Stay.
        let policy = flip_enabled();
        let lst = 12.0;
        let target_ra = lst + 3.0; // mech_HA = lst − ra = −3
        let chosen = select_pier_side_for_target(
            Ra::new(target_ra),
            Lst::new(lst),
            PierSide::West,
            &policy,
            BINDING_ZONE,
            LAT_NORTH,
        );
        assert_eq!(chosen, PierSide::West);
    }

    #[test]
    fn select_pier_side_north_piereast_inside_flip_window_stays() {
        // Currently pierEast (post-flip), target_HA inside the flip
        // window. Stay flipped (no unnecessary flip back).
        let policy = flip_enabled();
        let lst = 12.0;
        let target_ra = lst - 0.3; // mech_HA = +0.3 (within ±0.5)
        let chosen = select_pier_side_for_target(
            Ra::new(target_ra),
            Lst::new(lst),
            PierSide::East,
            &policy,
            BINDING_ZONE,
            LAT_NORTH,
        );
        assert_eq!(chosen, PierSide::East);
    }

    #[test]
    fn select_pier_side_north_piereast_outside_flip_window_flips_back_to_west() {
        // Currently pierEast (flipped), target outside the flip window
        // but inside the pre-flip envelope. Auto-flip back to pierWest
        // — the post-flip side cannot reach the target.
        let policy = flip_enabled();
        let lst = 12.0;
        let target_ra = lst + 3.0; // mech_HA = −3, outside ±0.5
        let chosen = select_pier_side_for_target(
            Ra::new(target_ra),
            Lst::new(lst),
            PierSide::East,
            &policy,
            BINDING_ZONE,
            LAT_NORTH,
        );
        assert_eq!(chosen, PierSide::West);
    }

    #[test]
    fn select_pier_side_north_pierwest_outside_pre_flip_envelope_flips_to_east() {
        // Currently pierWest, target HA past +6.95 (outside pre-flip
        // envelope and outside the flip window). Selector returns the
        // opposite side; subsequent envelope validation will reject the
        // slew with INVALID_VALUE regardless, but the prediction is
        // deterministic.
        let policy = flip_enabled();
        let lst = 12.0;
        let target_ra = lst - 7.5; // mech_HA = +7.5
        let chosen = select_pier_side_for_target(
            Ra::new(target_ra),
            Lst::new(lst),
            PierSide::West,
            &policy,
            BINDING_ZONE,
            LAT_NORTH,
        );
        assert_eq!(chosen, PierSide::East);
    }

    #[test]
    fn select_pier_side_north_flip_window_boundary_at_exact_flip_range_is_covered() {
        // |target_HA| = flip_range_hours exactly is on the *inclusive*
        // edge of the post-flip safe band — staying flipped is
        // valid. The slew planner's envelope check (issue-time)
        // matches this boundary handling.
        let policy = flip_enabled();
        let lst = 12.0;
        let target_ra = lst - 0.5; // mech_HA = +0.5 = flip_range_hours
        let chosen = select_pier_side_for_target(
            Ra::new(target_ra),
            Lst::new(lst),
            PierSide::East,
            &policy,
            BINDING_ZONE,
            LAT_NORTH,
        );
        assert_eq!(chosen, PierSide::East);
    }

    #[test]
    fn select_pier_side_north_target_on_zone_boundary_stays_natural_side() {
        // Open-interval semantics, consistent with
        // check_within_safe_envelope: a target landing exactly on a
        // binding-zone boundary is *not* in the zone, so the selector
        // keeps the natural (pre-flip) side instead of forcing a
        // spurious flip the envelope check would not require. Uses an
        // exactly-representable zone so the boundary equality is precise.
        let policy = flip_enabled();
        let lst = 12.0;
        let zone = (6.0, 10.0);
        for boundary in <[f64; 2]>::from(zone) {
            let target_ra = lst - boundary; // mech_HA = lst − ra = boundary, exact
            let chosen = select_pier_side_for_target(
                Ra::new(target_ra),
                Lst::new(lst),
                PierSide::West,
                &policy,
                zone,
                LAT_NORTH,
            );
            assert_eq!(
                chosen,
                PierSide::West,
                "target on zone boundary {boundary} must stay the natural side"
            );
        }
    }

    #[test]
    fn select_pier_side_southern_hemisphere_inverts_normal_side_label() {
        // In the Southern Hemisphere the "normal" pointing maps to
        // pierEast (Dec encoder within ±90° reads as East per the
        // existing side_of_pier convention). So a target inside the
        // pre-flip envelope on the current normal side means: current =
        // pierEast → stay pierEast. Mirror of the Northern test above.
        let policy = flip_enabled();
        let lst = 12.0;
        let target_ra = lst + 3.0; // mech_HA = −3
        let chosen = select_pier_side_for_target(
            Ra::new(target_ra),
            Lst::new(lst),
            PierSide::East,
            &policy,
            BINDING_ZONE,
            LAT_SOUTH,
        );
        assert_eq!(chosen, PierSide::East);
        // And the post-flip side (pierWest in south): outside flip
        // window means flip back to East (normal in south).
        let chosen = select_pier_side_for_target(
            Ra::new(target_ra),
            Lst::new(lst),
            PierSide::West,
            &policy,
            BINDING_ZONE,
            LAT_SOUTH,
        );
        assert_eq!(chosen, PierSide::East);
    }

    // ---------- Phase 6: encoder_to_celestial ----------

    #[test]
    fn encoder_to_celestial_pre_flip_inverts_pre_flip_pipeline() {
        // For pre-flip state (Dec encoder within ±90°), the helper
        // must invert target_encoder_normal exactly.
        let lst = 5.0;
        for (ra, dec) in [(12.0, 30.0), (3.5, -45.0), (0.0, 0.0), (23.9, 89.9)] {
            let (ra_ticks, dec_ticks) =
                target_encoder_normal(Ra::new(ra), Dec::new(dec), Lst::new(lst), cpr(), cpr());
            let (ra_back, dec_back) =
                encoder_to_celestial(ra_ticks, dec_ticks, Lst::new(lst), cpr(), cpr(), 47.6);
            let (ra_back, dec_back) = (ra_back.value(), dec_back.value());
            assert!(
                ((ra - ra_back + 12.0).rem_euclid(24.0) - 12.0).abs() < 1e-4,
                "ra round-trip: {ra} → {ra_back}"
            );
            assert!(
                (dec - dec_back).abs() < 1e-4,
                "dec round-trip: {dec} → {dec_back}"
            );
        }
    }

    #[test]
    fn encoder_to_celestial_post_flip_inverts_flipped_pipeline_north() {
        // Plan §2.0 worked example, LAT 45°N: target HA = −0.5, dec
        // = +45°. After a flip, the encoder lands at the flipped
        // position; encoder_to_celestial must recover the celestial
        // RA/Dec of the OTA pointing.
        let lst = 5.0;
        for (ra, dec) in [(lst + 0.5, 45.0), (lst - 0.3, 30.0), (lst, 0.0)] {
            let (ra_ticks, dec_ticks) =
                target_encoder_flipped(Ra::new(ra), Dec::new(dec), Lst::new(lst), cpr(), cpr());
            let (ra_back, dec_back) =
                encoder_to_celestial(ra_ticks, dec_ticks, Lst::new(lst), cpr(), cpr(), 47.6);
            let (ra_back, dec_back) = (ra_back.value(), dec_back.value());
            assert!(
                ((ra - ra_back + 12.0).rem_euclid(24.0) - 12.0).abs() < 1e-4,
                "post-flip ra round-trip: {ra} → {ra_back}"
            );
            assert!(
                (dec - dec_back).abs() < 1e-4,
                "post-flip dec round-trip: {dec} → {dec_back}"
            );
        }
    }

    #[test]
    fn encoder_to_celestial_post_flip_inverts_flipped_pipeline_south() {
        // Same flip round-trip but for a southern-hemisphere observer.
        // The pre-flip side label inverts (pierEast in south), so the
        // post-flip detection inverts too — `side_of_pier` does this
        // internally and `encoder_to_celestial` consumes it.
        let lst = 5.0;
        for (ra, dec) in [(lst + 0.5, 45.0), (lst, -30.0), (lst, 0.0)] {
            let (ra_ticks, dec_ticks) =
                target_encoder_flipped(Ra::new(ra), Dec::new(dec), Lst::new(lst), cpr(), cpr());
            let (ra_back, dec_back) =
                encoder_to_celestial(ra_ticks, dec_ticks, Lst::new(lst), cpr(), cpr(), -33.0);
            let (ra_back, dec_back) = (ra_back.value(), dec_back.value());
            assert!(
                ((ra - ra_back + 12.0).rem_euclid(24.0) - 12.0).abs() < 1e-4,
                "south post-flip ra round-trip: {ra} → {ra_back}"
            );
            assert!(
                (dec - dec_back).abs() < 1e-4,
                "south post-flip dec round-trip: {dec} → {dec_back}"
            );
        }
    }

    #[test]
    fn encoder_to_celestial_at_meridian_dec_zero_is_lst() {
        // Encoder at (0, 0) means mech_HA = 0 and dec encoder = 0 →
        // pre-flip pierWest in north → celestial (LST, 0).
        let lst = 7.25;
        let (ra, dec) = encoder_to_celestial(
            RaTicks::new(0),
            DecTicks::new(0),
            Lst::new(lst),
            cpr(),
            cpr(),
            47.6,
        );
        let (ra, dec) = (ra.value(), dec.value());
        assert!((ra - lst).abs() < 1e-6, "ra = {ra}, expected {lst}");
        assert!(dec.abs() < 1e-6, "dec = {dec}");
    }

    #[test]
    fn encoder_to_celestial_at_post_flip_wrap_dec_pole_recovers_zero_dec() {
        // Dec encoder at exactly +cpr/2 ticks (= ±180°, the encoder
        // wrap). `dec_ticks_to_degrees` folds to -180; side_of_pier
        // returns East in north (past 90°); celestial dec = -1 *
        // (180 - 180) = 0. RA mapping uses the post-flip +12h shift.
        let lst = 12.0;
        let half_cpr = (GTI_CPR / 2).cast_signed();
        let (_ra, dec) = encoder_to_celestial(
            RaTicks::new(0),
            DecTicks::new(half_cpr),
            Lst::new(lst),
            cpr(),
            cpr(),
            47.6,
        );
        let dec = dec.value();
        assert!(dec.abs() < 1e-6, "dec at wrap = {dec}");
    }

    #[test]
    fn opposite_pier_side_round_trips() {
        assert_eq!(opposite_pier_side(PierSide::West), PierSide::East);
        assert_eq!(opposite_pier_side(PierSide::East), PierSide::West);
        assert_eq!(opposite_pier_side(PierSide::Unknown), PierSide::Unknown);
    }

    #[test]
    fn sidereal_deg_per_sec_is_about_fifteen_arcseconds_per_second() {
        // Cross-check the constant against the textbook value: sidereal
        // rate ≈ 15.04108″/sec.
        let arcsec_per_sec = SIDEREAL_DEG_PER_SEC * 3600.0;
        assert!(
            (arcsec_per_sec - 15.04108).abs() < 1e-4,
            "expected ~15.04108″/sec, got {arcsec_per_sec}"
        );
    }
}

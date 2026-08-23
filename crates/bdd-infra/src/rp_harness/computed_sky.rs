//! A computed sky for planner-driven BDD scenarios.
//!
//! rp's `get_next_target` gates on real ephemeris: a target is viable
//! only when it sits above its altitude floor while the Sun is below
//! astronomical dusk (−18°) at the configured site — all evaluated at
//! wall-clock "now". A scenario that hard-codes a site and target would
//! therefore only pass at certain times of day. This module removes the
//! time dependence by *choosing the site to fit the clock*, in either
//! of two directions:
//!
//! - [`ComputedSky::night_at`] — an observer on the equator at the
//!   anti-solar longitude always has the Sun at lower culmination,
//!   altitude ≈ −(90° − |δ☉|) ≤ −66° — deep astronomical night at any
//!   moment of any date.
//! - [`ComputedSky::morning_at`] — an equatorial observer 45° west of
//!   the sub-solar longitude has local apparent solar time ≈ 09:00:
//!   the Sun is ≈ 40–45° up and climbing at roughly 13°/hour — a
//!   risen, unambiguous morning at any moment of any date. This is
//!   the lever for "the planner declares the night over" scenarios.
//!
//! Targets are then placed on the celestial equator by hour angle. At
//! an equatorial site a dec-0 target's altitude is 90° − 15°·|HA| and it
//! sinks at a constant 15°/sidereal hour (≈ 0.25°/min), which makes
//! "this target drops below its floor N seconds from now" exactly
//! computable — the lever the target-switch scenario uses.
//!
//! Everything is computed with the same `rp-ephemeris` implementation rp
//! itself uses, so the scenario's math and the planner's math cannot
//! disagree on model details (refraction, sidereal time).

use chrono::{DateTime, Utc};
use rp_ephemeris::{Ephemeris, ErfarsEphemeris, IcrsCoord, Site};

/// A site + instant computed so the Sun is where the scenario needs it
/// (deep night or risen morning), with helpers to place planner
/// targets by hour angle.
#[derive(Debug)]
pub struct ComputedSky {
    site: Site,
    now: DateTime<Utc>,
    eph: ErfarsEphemeris,
}

impl ComputedSky {
    /// Compute the equatorial anti-solar site for `now` — guaranteed
    /// deep astronomical night.
    ///
    /// The Sun transits (upper culmination) where local sidereal time
    /// equals its right ascension; the anti-solar meridian is 180°
    /// away. `lon/15 + GMST = RA☉` gives the sub-solar longitude, so
    /// the site longitude is `(RA☉ − GMST)·15 + 180`, normalised to
    /// Alpaca/ASCOM's ±180 convention. Latitude 0 puts the Sun's lower
    /// culmination at −90° + |δ☉| — never brighter than −66°.
    #[must_use]
    pub fn night_at(now: DateTime<Utc>) -> Self {
        Self::at_solar_offset(now, 180.0)
    }

    /// Compute an equatorial site 45° west of the sub-solar longitude
    /// for `now` — guaranteed risen morning.
    ///
    /// Local apparent solar time there is ≈ 09:00, so the Sun stands
    /// ≈ 40–45° above the horizon (declination shrinks it toward the
    /// solstices) and climbs toward its noon culmination — the
    /// planner's dawn-side trend check reads it as "the night is
    /// over" at any moment of any date.
    #[must_use]
    pub fn morning_at(now: DateTime<Utc>) -> Self {
        Self::at_solar_offset(now, -45.0)
    }

    /// The equatorial site whose longitude sits `offset_degrees` east
    /// of the sub-solar longitude at `now`.
    fn at_solar_offset(now: DateTime<Utc>, offset_degrees: f64) -> Self {
        let eph = ErfarsEphemeris::new();
        // Greenwich reference site: LST at longitude 0 is GMST.
        let greenwich = Site::new(0.0, 0.0).expect("the Greenwich reference site is valid");
        let gmst_hours = eph.sidereal_time(&greenwich, now).lst_hours;
        let sun_ra_hours = eph.sun_position(&greenwich, now).coords.ra_hours;

        let sub_solar_lon = (sun_ra_hours - gmst_hours) * 15.0;
        let mut lon = sub_solar_lon + offset_degrees;
        // Normalise to (−180, 180] for Site's range check.
        lon = lon.rem_euclid(360.0);
        if lon > 180.0 {
            lon -= 360.0;
        }
        let site = Site::new(0.0, lon).expect("an equatorial site with normalised longitude");
        Self { site, now, eph }
    }

    #[must_use]
    pub const fn latitude_degrees(&self) -> f64 {
        self.site.latitude_degrees
    }

    #[must_use]
    pub const fn longitude_degrees(&self) -> f64 {
        self.site.longitude_degrees
    }

    /// The Sun's altitude at the site at `now` — far below −18° for a
    /// night sky, well risen for a morning sky; exposed so tests can
    /// assert the construction.
    #[must_use]
    pub fn sun_altitude_degrees(&self) -> f64 {
        self.sun_altitude_degrees_in(0)
    }

    /// The Sun's altitude at the site `seconds` after `now` — the
    /// morning-sky tests assert the climb with it.
    ///
    /// # Panics
    ///
    /// Panics if `seconds` pushes the moment outside chrono's
    /// representable range.
    #[must_use]
    pub fn sun_altitude_degrees_in(&self, seconds: i64) -> f64 {
        let at = self
            .now
            .checked_add_signed(chrono::Duration::seconds(seconds))
            .expect("test-scale offset stays within the chrono range");
        self.eph
            .sun_position(&self.site, at)
            .alt_az
            .altitude_degrees
    }

    /// A celestial-equator target at the given signed hour angle at
    /// `now` (negative = east of the meridian / rising toward transit,
    /// positive = west / descending — rp's `signed_hour_angle`
    /// convention). Its altitude is 90° − 15°·|HA| and it sinks (for
    /// positive HA) at a constant ≈ 0.25°/minute.
    #[must_use]
    pub fn target_at_hour_angle(&self, ha_hours: f64) -> IcrsCoord {
        let lst = self.eph.sidereal_time(&self.site, self.now).lst_hours;
        IcrsCoord {
            ra_hours: (lst - ha_hours).rem_euclid(24.0),
            dec_degrees: 0.0,
        }
    }

    /// The altitude `target` will have `seconds` after `now`, from the
    /// same alt/az transform (refraction included) rp's planner uses.
    /// Setting this as a descending target's `min_altitude_degrees`
    /// makes the planner drop it at exactly that moment.
    ///
    /// # Panics
    ///
    /// Panics if `seconds` pushes the moment outside chrono's
    /// representable range, or if the ephemeris rejects the resulting
    /// time — which it does not for test-scale offsets from a valid
    /// `now`.
    #[must_use]
    pub fn altitude_degrees_in(&self, target: IcrsCoord, seconds: i64) -> f64 {
        let at = self
            .now
            .checked_add_signed(chrono::Duration::seconds(seconds))
            .expect("test-scale offset stays within the chrono range");
        self.eph
            .alt_az(&self.site, target, at)
            .expect("alt/az is defined away from degenerate sites")
            .altitude_degrees
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn moments() -> Vec<DateTime<Utc>> {
        // Solstices, equinox, and odd hours across the day — the
        // computed sites must hold their guarantee at all of them.
        vec![
            Utc.with_ymd_and_hms(2026, 6, 21, 12, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 12, 21, 0, 30, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 3, 20, 17, 45, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 9, 23, 6, 10, 0).unwrap(),
            Utc.with_ymd_and_hms(2027, 1, 5, 21, 59, 59).unwrap(),
        ]
    }

    #[test]
    fn the_sun_is_in_deep_astronomical_night_at_every_moment() {
        for now in moments() {
            let sky = ComputedSky::night_at(now);
            let sun_alt = sky.sun_altitude_degrees();
            assert!(
                sun_alt < -60.0,
                "expected the Sun far below astronomical dusk at {now}, got {sun_alt}°"
            );
        }
    }

    #[test]
    fn the_morning_sun_is_well_risen_and_climbing_at_every_moment() {
        for now in moments() {
            let sky = ComputedSky::morning_at(now);
            let sun_alt = sky.sun_altitude_degrees();
            assert!(
                sun_alt > 35.0,
                "expected a well-risen morning Sun at {now}, got {sun_alt}°"
            );
            let climb = sky.sun_altitude_degrees_in(600) - sun_alt;
            assert!(
                climb > 1.0,
                "expected the morning Sun to climb ≈ 2° over 600 s at {now}, got {climb}°"
            );
        }
    }

    #[test]
    fn the_sites_are_equatorial_with_in_range_longitude() {
        for now in moments() {
            for sky in [ComputedSky::night_at(now), ComputedSky::morning_at(now)] {
                assert_eq!(sky.latitude_degrees(), 0.0);
                assert!(
                    (-180.0..=180.0).contains(&sky.longitude_degrees()),
                    "longitude out of Site range at {now}: {}",
                    sky.longitude_degrees()
                );
            }
        }
    }

    #[test]
    fn a_target_half_an_hour_past_transit_stands_near_82_degrees() {
        for now in moments() {
            let sky = ComputedSky::night_at(now);
            let target = sky.target_at_hour_angle(0.5);
            let alt = sky.altitude_degrees_in(target, 0);
            // 90 − 15·0.5 = 82.5°; refraction adds well under a degree.
            assert!(
                (81.5..=83.5).contains(&alt),
                "expected ≈82.5° at {now}, got {alt}°"
            );
        }
    }

    #[test]
    fn a_descending_target_sinks_a_quarter_degree_per_minute() {
        let sky = ComputedSky::night_at(Utc.with_ymd_and_hms(2026, 7, 7, 3, 0, 0).unwrap());
        let target = sky.target_at_hour_angle(3.0);
        let now_alt = sky.altitude_degrees_in(target, 0);
        let later_alt = sky.altitude_degrees_in(target, 120);
        let sunk = now_alt - later_alt;
        assert!(
            (0.4..=0.6).contains(&sunk),
            "expected ≈0.5° of sink over 120s at 45° altitude, got {sunk}°"
        );
    }

    #[test]
    fn hour_angle_zero_is_the_zenith_at_an_equatorial_site() {
        let sky = ComputedSky::night_at(Utc.with_ymd_and_hms(2026, 7, 7, 3, 0, 0).unwrap());
        let target = sky.target_at_hour_angle(0.0);
        let alt = sky.altitude_degrees_in(target, 0);
        assert!(alt > 89.0, "expected the zenith, got {alt}°");
    }
}

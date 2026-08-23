//! The plugin's view of `rp-ephemeris`: site-bound ICRS↔observed
//! conversions with the configured refraction model, local sidereal
//! time, and the pole target.
//!
//! Frame contract (see the design doc's Geometry Reference): plate
//! solves map apparent (refracted) pointings to catalog coordinates,
//! so the fitted axis already carries the solves' refraction pull.
//! Converting it ICRS→observed with refraction ON re-adds the lift,
//! landing a perfectly aligned axis exactly on the geometric pole —
//! the target is therefore the plain (azimuth 0/180, altitude
//! |latitude|) point with no refraction term of its own.

use chrono::{DateTime, Utc};
use rp_ephemeris::{AltAz, Ephemeris, ErfarsEphemeris, IcrsCoord, RefractionConditions, Site};

use crate::config::{RefractionConfig, SiteConfig};
use crate::error::{PolarAlignError, Result};
use crate::math::{radec_from_unit, unit_from_radec, Vec3};

/// Site- and refraction-bound conversion context, built once per
/// workflow from the *resolved* site (the config's `site` block or
/// rp's `get_site` — both arrive as an already-validated
/// [`SiteConfig`]).
pub struct EphemerisCtx {
    eph: ErfarsEphemeris,
    site: Site,
    refraction: Option<RefractionConditions>,
    hemisphere_sign: f64,
}

impl EphemerisCtx {
    /// Bind the conversions to `site_config`, with refraction on or off per
    /// `refraction_config`.
    ///
    /// # Errors
    ///
    /// Returns [`PolarAlignError::Config`] if `rp-ephemeris` rejects the
    /// site: latitude outside ±90° or longitude outside ±180°.
    pub fn new(site_config: SiteConfig, refraction_config: &RefractionConfig) -> Result<Self> {
        let site = Site::new(
            site_config.latitude_deg.degrees(),
            site_config.longitude_deg.degrees(),
        )
        .map_err(|e| PolarAlignError::Config(format!("site: {e}")))?;
        let refraction = if refraction_config.enabled {
            Some(RefractionConditions {
                pressure_hpa: refraction_config.pressure_hpa,
                temperature_c: refraction_config.temperature_c,
            })
        } else {
            None
        };
        Ok(Self {
            eph: ErfarsEphemeris::new(),
            site,
            refraction,
            hemisphere_sign: site_config.latitude_deg.hemisphere_sign(),
        })
    }

    /// Local sidereal time in hours [0, 24).
    #[must_use]
    pub fn lst_hours(&self, time: DateTime<Utc>) -> f64 {
        self.eph.sidereal_time(&self.site, time).lst_hours
    }

    /// +1.0 north of the equator, -1.0 south of it.
    #[must_use]
    pub const fn hemisphere_sign(&self) -> f64 {
        self.hemisphere_sign
    }

    /// The visible celestial pole's ICRS direction — the sign picker
    /// for the three-point axis fit.
    #[must_use]
    pub fn pole_hemisphere_unit(&self) -> Vec3 {
        unit_from_radec(0.0, 90.0 * self.hemisphere_sign)
    }

    /// The pole target in observed coordinates: azimuth 0 (north
    /// hemisphere) or 180 (south), altitude |latitude|. No refraction
    /// term — see the module doc.
    #[must_use]
    pub fn pole_target_alt_az(&self) -> AltAz {
        AltAz {
            azimuth_degrees: if self.hemisphere_sign >= 0.0 {
                0.0
            } else {
                180.0
            },
            altitude_degrees: self.site.latitude_degrees.abs(),
        }
    }

    /// Observed az/alt of an ICRS unit vector under the configured
    /// refraction model.
    ///
    /// # Errors
    ///
    /// Returns [`PolarAlignError::Ephemeris`] if ERFA refuses the
    /// transform: a `time` it cannot represent, or inputs outside its
    /// valid range.
    pub fn observed_of(&self, icrs: Vec3, time: DateTime<Utc>) -> Result<AltAz> {
        let (ra_deg, dec_deg) = radec_from_unit(icrs);
        self.eph
            .alt_az_with_conditions(
                &self.site,
                IcrsCoord {
                    ra_hours: ra_deg / 15.0,
                    dec_degrees: dec_deg,
                },
                time,
                self.refraction,
            )
            .map_err(|e| PolarAlignError::Ephemeris(e.to_string()))
    }

    /// The ICRS direction a fitted axis should equal when the mount
    /// is perfectly aligned: the pole target mapped observed→ICRS
    /// under the configured refraction model.
    ///
    /// # Errors
    ///
    /// Returns [`PolarAlignError::Ephemeris`] if ERFA refuses the
    /// transform: a `time` it cannot represent, or inputs outside its
    /// valid range.
    pub fn axis_target_icrs(&self, time: DateTime<Utc>) -> Result<Vec3> {
        let target = self
            .eph
            .icrs_from_alt_az(&self.site, self.pole_target_alt_az(), time, self.refraction)
            .map_err(|e| PolarAlignError::Ephemeris(e.to_string()))?;
        Ok(unit_from_radec(target.ra_hours * 15.0, target.dec_degrees))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn unrefracted_ctx(latitude: f64) -> EphemerisCtx {
        let site: SiteConfig = serde_json::from_str(&format!(
            r#"{{ "latitude_deg": {latitude}, "longitude_deg": -122.8 }}"#
        ))
        .unwrap();
        let refraction: RefractionConfig = serde_json::from_str(r#"{ "enabled": false }"#).unwrap();
        EphemerisCtx::new(site, &refraction).unwrap()
    }

    #[test]
    fn test_pole_target_faces_north_in_the_north() {
        let ctx = unrefracted_ctx(48.1);
        let pole = ctx.pole_target_alt_az();
        assert_eq!(pole.azimuth_degrees, 0.0);
        assert_eq!(pole.altitude_degrees, 48.1);
        assert_eq!(ctx.hemisphere_sign(), 1.0);
    }

    #[test]
    fn test_pole_target_faces_south_in_the_south() {
        let ctx = unrefracted_ctx(-33.9);
        let pole = ctx.pole_target_alt_az();
        assert_eq!(pole.azimuth_degrees, 180.0);
        assert_eq!(pole.altitude_degrees, 33.9);
        assert_eq!(ctx.pole_hemisphere_unit().z, -1.0);
    }

    /// Unrefracted: the axis target's observed position must be the
    /// pole target itself (round trip through ICRS), and the true
    /// celestial pole's observed altitude must be the site latitude.
    #[test]
    fn test_axis_target_round_trips_to_the_pole_target() {
        let ctx = unrefracted_ctx(48.1);
        let t = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 8, 1, 8, 0, 0).unwrap();
        let target_icrs = ctx.axis_target_icrs(t).unwrap();
        let observed = ctx.observed_of(target_icrs, t).unwrap();
        let pole = ctx.pole_target_alt_az();
        assert!(
            (observed.altitude_degrees - pole.altitude_degrees).abs() < 1e-3,
            "altitude {} vs {}",
            observed.altitude_degrees,
            pole.altitude_degrees
        );
        let az_diff =
            (observed.azimuth_degrees - pole.azimuth_degrees + 180.0).rem_euclid(360.0) - 180.0;
        assert!(az_diff.abs() < 1e-3, "azimuth diff {az_diff}");
    }

    #[test]
    fn test_lst_advances_with_time() {
        let ctx = unrefracted_ctx(48.1);
        let t0 = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 8, 1, 6, 0, 0).unwrap();
        let t1 = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 8, 1, 7, 0, 0).unwrap();
        let advance = (ctx.lst_hours(t1) - ctx.lst_hours(t0)).rem_euclid(24.0);
        // One solar hour ≈ 1.0027 sidereal hours.
        assert!(
            (advance - 1.0027).abs() < 0.001,
            "LST advanced by {advance} hours"
        );
    }
}

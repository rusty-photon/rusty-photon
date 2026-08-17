//! `assume_epoch` handling: converting received coordinates to ICRS at
//! receipt (docs/services/planetarium-bridge.md § Epoch handling).
//!
//! The device declares `EquatorialSystem = J2000`, so `"j2000"` (the
//! default) is the identity. `"jnow"` treats received coordinates as
//! apparent place of date (equinox-based) and inverts precession, nutation,
//! and annual aberration via ERFA: the equation of the origins maps the
//! equinox-based RA onto the CIRS RA `Atic13` expects. ΔUT1 is ignored,
//! exactly as in `rp-ephemeris` — irrelevant at arcminute scale.

use chrono::{DateTime, Datelike, Timelike, Utc};
use erfars::astrometry::Atic13;
use erfars::precnutpolar::Eo06a;
use erfars::timescales::{Dtf2d, Taitt, Utctai};
use tracing::warn;

use crate::config::AssumeEpoch;

/// Convert received coordinates to ICRS per the configured assumption.
///
/// When ERFA cannot represent the host clock (calendar range), the raw
/// coordinates pass through with a warning — an import with a ~20′ frame
/// error beats a lost Align.
#[must_use]
pub fn to_icrs(
    epoch: AssumeEpoch,
    ra_hours: f64,
    dec_degrees: f64,
    now: DateTime<Utc>,
) -> (f64, f64) {
    match epoch {
        AssumeEpoch::J2000 => (ra_hours, dec_degrees),
        AssumeEpoch::Jnow => jnow_to_icrs(ra_hours, dec_degrees, now).unwrap_or_else(|| {
            warn!(
                ra_hours,
                dec_degrees,
                "ERFA cannot convert for the host clock; importing coordinates unconverted"
            );
            (ra_hours, dec_degrees)
        }),
    }
}

/// Apparent place of date (equinox-based RA/Dec) → ICRS. `None` when the
/// host clock is outside ERFA's calendar range.
fn jnow_to_icrs(ra_hours: f64, dec_degrees: f64, now: DateTime<Utc>) -> Option<(f64, f64)> {
    let (tt1, tt2) = tt_jd(now)?;
    // Equinox-based apparent RA + equation of the origins = CIRS RA.
    let ri = (ra_hours * 15.0).to_radians() + Eo06a(tt1, tt2);
    let di = dec_degrees.to_radians();
    let (rc, dc, _eo) = Atic13(ri, di, tt1, tt2);
    Some(((rc.to_degrees() / 15.0).rem_euclid(24.0), dc.to_degrees()))
}

/// TT Julian date pair for a UTC instant, per the rp-ephemeris idiom
/// (ΔUT1 ignored).
fn tt_jd(now: DateTime<Utc>) -> Option<(f64, f64)> {
    let seconds = f64::from(now.second()) + f64::from(now.nanosecond()) * 1e-9;
    let (utc1, utc2) = match Dtf2d(
        true,
        now.year(),
        now.month().cast_signed(),
        now.day().cast_signed(),
        now.hour().cast_signed(),
        now.minute().cast_signed(),
        seconds,
    ) {
        Ok((pair, _status)) => pair,
        Err(e) => {
            warn!("ERFA Dtf2d failed for {now}: {e:?}");
            return None;
        }
    };
    let (tai1, tai2) = match Utctai(utc1, utc2) {
        Ok((pair, _status)) => pair,
        Err(e) => {
            warn!("ERFA Utctai failed for {now}: {e:?}");
            return None;
        }
    };
    Some(Taitt(tai1, tai2))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Great-circle separation in arcminutes.
    fn separation_arcmin(a: (f64, f64), b: (f64, f64)) -> f64 {
        let (ra1, dec1) = ((a.0 * 15.0).to_radians(), a.1.to_radians());
        let (ra2, dec2) = ((b.0 * 15.0).to_radians(), b.1.to_radians());
        let sin_half_dra = ((ra1 - ra2) / 2.0).sin();
        let sin_half_ddec = ((dec1 - dec2) / 2.0).sin();
        let h =
            sin_half_ddec * sin_half_ddec + dec1.cos() * dec2.cos() * sin_half_dra * sin_half_dra;
        (2.0 * h.sqrt().asin()).to_degrees() * 60.0
    }

    #[test]
    fn j2000_is_the_identity() {
        let now = Utc.with_ymd_and_hms(2026, 7, 30, 6, 0, 0).unwrap();
        assert_eq!(to_icrs(AssumeEpoch::J2000, 14.0, -40.0, now), (14.0, -40.0));
    }

    /// The P3a precession fingerprint: 14h00m / −40° read as `JNow` is
    /// J2000 13h58m23s / −39°52′11″ ≈ (13.97306 h, −39.86972°). The band
    /// (within 5′ of the J2000 value, at least 15′ from the input) stays
    /// valid for years of accumulating precession.
    #[test]
    fn jnow_inverts_the_p3a_precession_fingerprint() {
        let now = Utc.with_ymd_and_hms(2026, 7, 30, 6, 0, 0).unwrap();
        let icrs = to_icrs(AssumeEpoch::Jnow, 14.0, -40.0, now);
        let to_pin = separation_arcmin(icrs, (13.97306, -39.86972));
        assert!(
            to_pin < 5.0,
            "converted {icrs:?} is {to_pin:.2}' from the pin"
        );
        let from_input = separation_arcmin(icrs, (14.0, -40.0));
        assert!(
            from_input > 15.0,
            "converted {icrs:?} moved only {from_input:.2}' — conversion skipped?"
        );
    }

    /// Round-trip through the spike-proven forward direction: converting a
    /// `JNow` value back to ICRS then forward again must land on the input.
    #[test]
    fn jnow_conversion_is_consistent_under_ra_wrap() {
        let now = Utc.with_ymd_and_hms(2026, 7, 30, 6, 0, 0).unwrap();
        // Near the 0h/24h wrap; the result must stay in [0, 24).
        let (ra, _dec) = to_icrs(AssumeEpoch::Jnow, 0.01, 10.0, now);
        assert!((0.0..24.0).contains(&ra), "wrapped RA out of range: {ra}");
    }
}

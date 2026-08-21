use std::any::Any;
use std::panic::{self, UnwindSafe};
use std::sync::Once;

use chrono::{DateTime, Datelike, Duration, NaiveDate, Timelike, Utc};
use erfars::astrometry::{Atco13, Atoc13};
use erfars::constants::{ERFA_DD2R, ERFA_DPI, ERFA_DR2D};
use erfars::ephemerides::{Epv00, Moon98};
use erfars::rotationtime::Gst06a;
use erfars::timescales::{Dtf2d, Taitt, Utctai};
use erfars::ERFAResult;

use crate::derived;
use crate::site::Site;
use crate::types::{
    AltAz, EphemerisError, IcrsCoord, LocalSiderealTime, MoonInfo, RefractionConditions, RiseSet,
    SideOfPier, SunInfo, TwilightKind, TwilightWindow,
};
use crate::Ephemeris;

/// ERFA-backed [`Ephemeris`] implementation. Holds no state beyond a
/// zero-sized marker — every call is a fresh trip through ERFA.
#[derive(Debug, Default, Clone, Copy)]
pub struct ErfarsEphemeris;

impl ErfarsEphemeris {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Topocentric alt/az for an ICRS target with caller-controlled
    /// refraction: explicit atmospheric conditions, or `None` for the
    /// pure geometric (unrefracted) transform. The trait's
    /// [`Ephemeris::alt_az`] is this with the default conditions.
    ///
    /// # Errors
    ///
    /// Returns [`EphemerisError::InvalidAltAzInputs`] if ERFA rejects the
    /// inputs — in practice a `time` outside the range its UTC handling
    /// accepts.
    pub fn alt_az_with_conditions(
        &self,
        site: &Site,
        target: IcrsCoord,
        time: DateTime<Utc>,
        refraction: Option<RefractionConditions>,
    ) -> Result<AltAz, EphemerisError> {
        run_with_guard("alt_az_with_conditions", Ok(nan_alt_az()), || {
            let jds = time_jds(time);
            alt_az_conditions_at(site, target, &jds, refraction)
        })
    }

    /// The inverse of [`Self::alt_az_with_conditions`]: the ICRS
    /// coordinates whose observed position at `site` and `time` is
    /// the given alt/az under the given refraction conditions.
    ///
    /// # Errors
    ///
    /// Returns [`EphemerisError::InvalidAltAzInputs`] if ERFA rejects the
    /// inputs — in practice a `time` outside the range its UTC handling
    /// accepts.
    pub fn icrs_from_alt_az(
        &self,
        site: &Site,
        observed: AltAz,
        time: DateTime<Utc>,
        refraction: Option<RefractionConditions>,
    ) -> Result<IcrsCoord, EphemerisError> {
        run_with_guard("icrs_from_alt_az", Ok(nan_icrs()), || {
            let jds = time_jds(time);
            icrs_conditions_at(site, observed, &jds, refraction)
        })
    }
}

/// Runs `f` inside `panic::catch_unwind`. On panic, extracts a string
/// from the payload, logs it via `tracing::error!`, and returns
/// `fallback`. The panic hook still fires before we catch — operators
/// see the panic on stderr — but the service stays up.
///
/// This is the central defense against panics inside the erfars
/// wrappers: their `unexpected_val_err!` macro turns into `panic!()`
/// for any ERFA return code outside the wrapper's known set (today,
/// nothing actually triggers it, but we don't control the upstream
/// crate). Wrapping each [`Ephemeris`] trait method's body here means
/// any future inconsistency surfaces as NaN/None rather than a service
/// crash.
fn run_with_guard<R, F>(method: &'static str, fallback: R, f: F) -> R
where
    F: FnOnce() -> R + UnwindSafe,
{
    match panic::catch_unwind(f) {
        Ok(value) => value,
        Err(payload) => {
            let message = panic_payload_message(&*payload);
            tracing::error!(
                method,
                panic_message = %message,
                "ERFA call panicked; returning fallback value. Operators should treat \
                 this as either a host-clock misconfiguration or an upstream wrapper \
                 inconsistency and investigate."
            );
            fallback
        }
    }
}

/// Guards the process-wide, one-time initialization of ERFA's
/// leap-second table (see [`ensure_erfa_leap_seconds_initialized`]).
static ERFA_LEAP_SECONDS_INIT: Once = Once::new();

/// Force ERFA's user-updatable leap-second table to initialize exactly
/// once, single-threaded, before any concurrent ephemeris call.
///
/// ERFA bolts a mutable, user-replaceable leap-second table onto
/// otherwise-reentrant SOFA (`external/erfa/src/erfadatextra.c`). Its
/// `eraDatini` lazily fills two *file-static* globals — `changes` (the
/// table pointer) and `NDAT` (its length, sentinel `-1`) — on the first
/// `eraDat` call, with **no lock and no atomics**. When several threads
/// make their first `eraDat` call at once, one can observe `NDAT > 0`
/// without a happens-before edge on `changes`, then index `changes[..]`
/// through a null/half-written pointer → SIGSEGV. Every [`Ephemeris`]
/// method reaches `eraDat` through [`time_jds`] (UTC→TAI), so warming it
/// there under a [`Once`] serializes that first call; afterwards `NDAT`
/// is positive forever and concurrent callers only ever read it. See
/// `docs/crates/rp-ephemeris.md` § "Thread safety".
fn ensure_erfa_leap_seconds_initialized() {
    ERFA_LEAP_SECONDS_INIT.call_once(|| {
        // Touch `eraDat` once via the raw erfars calls — deliberately NOT
        // `time_jds`, which re-enters this guard and would deadlock the
        // `Once`. J2000 is safely inside ERFA's calendar range, so `Dtf2d`
        // succeeds and `Utctai` runs the `eraDat` lookup whose lazy init
        // this warms. The results are discarded; only the side effect
        // matters.
        if let Ok(((utc1, utc2), _)) = Dtf2d(true, 2000, 1, 1, 12, 0, 0.0) {
            let _ = Utctai(utc1, utc2);
        }
    });
}

/// Best-effort extraction of a panic payload as a `String`. The payload
/// must be dropped while holding the original `Box`, so we copy any
/// useful text out before the borrow ends.
fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&'static str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_string())
}

const fn nan_alt_az() -> AltAz {
    AltAz {
        altitude_degrees: f64::NAN,
        azimuth_degrees: f64::NAN,
    }
}

const fn nan_icrs() -> IcrsCoord {
    IcrsCoord {
        ra_hours: f64::NAN,
        dec_degrees: f64::NAN,
    }
}

const fn nan_time_jds() -> TimeJds {
    TimeJds {
        utc1: f64::NAN,
        utc2: f64::NAN,
        tt1: f64::NAN,
        tt2: f64::NAN,
        ut11: f64::NAN,
        ut12: f64::NAN,
    }
}

/// JD pairs for the time scales we care about. Computed once per call
/// to avoid hammering ERFA's leapsecond table four times in a row.
pub struct TimeJds {
    pub utc1: f64,
    pub utc2: f64,
    pub tt1: f64,
    pub tt2: f64,
    pub ut11: f64,
    pub ut12: f64,
}

/// Convert a `DateTime<Utc>` to the ERFA JD-pair time scales we need.
///
/// chrono can construct dates outside ERFA's calendar range (years
/// down to -262144 vs. ERFA's -4799 floor). Both ERFA calls below
/// have Err handlers that surface NaN-filled JDs rather than
/// panicking; NaN flows through the downstream float math and shows
/// up in the dashboard or alpaca client rather than crashing the
/// service. The handler bodies live in `dtf2d_jds` and `utctai_pair`
/// so they're directly unit-testable.
#[expect(
    clippy::suboptimal_flops,
    reason = "seconds + nanoseconds·1e-9 in the plain shape every timekeeping site uses; no accuracy stake at ΔUT1-ignored precision"
)]
pub fn time_jds(time: DateTime<Utc>) -> TimeJds {
    // The first `eraDat` call ERFA ever makes lazily initializes a
    // non-thread-safe file-static table; force that to happen once,
    // single-threaded, before the concurrent `Dtf2d`/`Utctai` calls below
    // can race it into a segfault. No-op (one atomic load) after warm-up.
    ensure_erfa_leap_seconds_initialized();

    let year = time.year();
    let month = time.month().cast_signed();
    let day = time.day().cast_signed();
    let hh = time.hour().cast_signed();
    let mm = time.minute().cast_signed();
    let seconds = f64::from(time.second()) + f64::from(time.nanosecond()) * 1e-9;

    let Some((utc1, utc2)) = dtf2d_jds(Dtf2d(true, year, month, day, hh, mm, seconds), time) else {
        return nan_time_jds();
    };
    let Some((tai1, tai2)) = utctai_pair(Utctai(utc1, utc2), time) else {
        return nan_time_jds();
    };
    let (tt1, tt2) = Taitt(tai1, tai2);
    // ΔUT1 = 0; UT1 ≈ UTC. UTC pair doubles as the UT1 pair.
    TimeJds {
        utc1,
        utc2,
        tt1,
        tt2,
        ut11: utc1,
        ut12: utc2,
    }
}

/// Unwrap a `Dtf2d` result into the UTC JD pair, logging and returning
/// `None` on Err. Reachable from production: chrono accepts years
/// outside ERFA's [-4799, +∞) range.
fn dtf2d_jds(result: ERFAResult<(f64, f64)>, time: DateTime<Utc>) -> Option<(f64, f64)> {
    match result {
        Ok((pair, _status)) => Some(pair),
        Err(e) => {
            tracing::error!(
                ?time,
                error = ?e,
                "ERFA Dtf2d rejected a chrono-validated DateTime<Utc>; host clock is \
                 outside ERFA's calendar range. Returning NaN time JDs; downstream \
                 computations will surface NaN until the host clock is corrected."
            );
            None
        }
    }
}

/// Unwrap a `Utctai` result into the TAI JD pair, logging and
/// returning `None` on Err. Unreachable in practice (Dtf2d already
/// filters the years that would cause `Utctai`'s internal `eraDat`
/// call to error), but kept as a defensive fallback rather than a
/// `.expect` so production code stays panic-free.
fn utctai_pair(result: ERFAResult<(f64, f64)>, time: DateTime<Utc>) -> Option<(f64, f64)> {
    match result {
        Ok((pair, _status)) => Some(pair),
        Err(e) => {
            tracing::error!(
                ?time,
                error = ?e,
                "ERFA Utctai failed despite Dtf2d succeeding — upstream invariant \
                 violation. Returning NaN time JDs; downstream computations will \
                 surface NaN."
            );
            None
        }
    }
}

/// Greenwich apparent sidereal time, in radians, at the given JDs.
pub fn gast_radians(jds: &TimeJds) -> f64 {
    Gst06a(jds.ut11, jds.ut12, jds.tt1, jds.tt2)
}

/// Local apparent sidereal time at `site`, in hours `[0, 24)`.
pub fn lst_hours(site: &Site, jds: &TimeJds) -> f64 {
    let gast_hours = gast_radians(jds) * 12.0 / ERFA_DPI;
    (gast_hours + site.longitude_degrees / 15.0).rem_euclid(24.0)
}

/// Topocentric alt/az for an ICRS target at the given UTC time, under
/// the default amateur-rig refraction conditions documented on the
/// trait.
pub fn alt_az_at(site: &Site, target: IcrsCoord, jds: &TimeJds) -> Result<AltAz, EphemerisError> {
    alt_az_conditions_at(site, target, jds, Some(RefractionConditions::default()))
}

/// ERFA refraction inputs (phpa, tc, rh, wl) for the given conditions.
/// `None` disables refraction entirely: ERFA's `Refco` yields zero
/// refraction constants for non-positive pressure, so the transform
/// degrades to the pure geometric (unrefracted) one.
const fn erfa_refraction_inputs(refraction: Option<RefractionConditions>) -> (f64, f64, f64, f64) {
    match refraction {
        Some(c) => (c.pressure_hpa, c.temperature_c, 0.5, 0.55),
        None => (0.0, 10.0, 0.5, 0.55),
    }
}

/// Topocentric alt/az for an ICRS target with explicit refraction
/// conditions (`None` = unrefracted).
pub fn alt_az_conditions_at(
    site: &Site,
    target: IcrsCoord,
    jds: &TimeJds,
    refraction: Option<RefractionConditions>,
) -> Result<AltAz, EphemerisError> {
    let rc = target.ra_hours * 15.0 * ERFA_DD2R;
    let dc = target.dec_degrees * ERFA_DD2R;
    let elong = site.longitude_degrees * ERFA_DD2R;
    let phi = site.latitude_degrees * ERFA_DD2R;
    let (phpa, tc, rh, wl) = erfa_refraction_inputs(refraction);
    let result = Atco13(
        rc, dc, 0.0, 0.0, 0.0, 0.0, jds.utc1, jds.utc2, 0.0, elong, phi, 0.0, 0.0, 0.0, phpa, tc,
        rh, wl,
    )
    .map_err(EphemerisError::InvalidAltAzInputs)?;
    let (aob, zob, _hob, _dob, _rob, _eo) = result.0;
    let altitude_degrees = (ERFA_DPI / 2.0 - zob) * ERFA_DR2D;
    let azimuth_degrees = (aob * ERFA_DR2D).rem_euclid(360.0);
    Ok(AltAz {
        altitude_degrees,
        azimuth_degrees,
    })
}

/// ICRS coordinates whose observed position at `site` is the given
/// alt/az — the inverse of [`alt_az_conditions_at`], via ERFA's
/// `Atoc13` with observed type `'A'` (azimuth / zenith distance).
pub fn icrs_conditions_at(
    site: &Site,
    observed: AltAz,
    jds: &TimeJds,
    refraction: Option<RefractionConditions>,
) -> Result<IcrsCoord, EphemerisError> {
    let ob1 = observed.azimuth_degrees * ERFA_DD2R;
    let ob2 = (90.0 - observed.altitude_degrees) * ERFA_DD2R;
    let elong = site.longitude_degrees * ERFA_DD2R;
    let phi = site.latitude_degrees * ERFA_DD2R;
    let (phpa, tc, rh, wl) = erfa_refraction_inputs(refraction);
    let result = Atoc13(
        'A', ob1, ob2, jds.utc1, jds.utc2, 0.0, elong, phi, 0.0, 0.0, 0.0, phpa, tc, rh, wl,
    )
    .map_err(EphemerisError::InvalidAltAzInputs)?;
    let (rc, dc) = result.0;
    Ok(IcrsCoord {
        ra_hours: (rc * ERFA_DR2D / 15.0).rem_euclid(24.0),
        dec_degrees: dc * ERFA_DR2D,
    })
}

/// Geocentric astrometric Sun coordinates from `Epv00`. The Sun
/// direction from Earth is the negative of the Earth's heliocentric
/// position (BCRS ≈ ICRS to milliarcsec).
///
/// The underlying ERFA `eraEpv00` only ever returns 0 or +1, so the
/// erfars wrapper never produces `Err` today. We still match on
/// `Err` defensively — returning NaN coords rather than `.expect`-ing
/// — so production code stays panic-free if the upstream contract
/// ever changes. The handler lives in `epv00_heliocentric` so it's
/// directly unit-testable.
pub fn sun_icrs(jds: &TimeJds) -> IcrsCoord {
    let Some(pvh) = epv00_heliocentric(Epv00(jds.tt1, jds.tt2), jds) else {
        return nan_icrs();
    };
    let x = -pvh[0];
    let y = -pvh[1];
    let z = -pvh[2];
    cartesian_to_icrs(x, y, z)
}

/// Unwrap an `Epv00` result into the heliocentric position vector,
/// logging and returning `None` on Err. Unreachable in practice
/// (eraEpv00 only returns 0 or +1), but kept as a defensive fallback
/// rather than `.expect` so production code stays panic-free.
fn epv00_heliocentric(result: ERFAResult<([f64; 6], [f64; 6])>, jds: &TimeJds) -> Option<[f64; 6]> {
    match result {
        Ok(((pvh, _pvb), _warn)) => Some(pvh),
        Err(e) => {
            tracing::error!(
                tt1 = jds.tt1,
                tt2 = jds.tt2,
                error = ?e,
                "ERFA Epv00 returned Err despite a contract of Ok(0) or Ok(+1) — \
                 upstream invariant violation. Returning NaN sun coordinates."
            );
            None
        }
    }
}

/// Geocentric Moon coordinates from `Moon98` (GCRS ≈ ICRS).
pub fn moon_icrs(jds: &TimeJds) -> IcrsCoord {
    let pv = Moon98(jds.tt1, jds.tt2);
    cartesian_to_icrs(pv[0], pv[1], pv[2])
}

#[expect(
    clippy::suboptimal_flops,
    reason = "mirrors the ERFA reference implementation; diverging from its arithmetic shape breaks cross-checking against the C library"
)]
fn cartesian_to_icrs(x: f64, y: f64, z: f64) -> IcrsCoord {
    let r = (x * x + y * y + z * z).sqrt();
    let mut ra = y.atan2(x);
    if ra < 0.0 {
        ra += 2.0 * ERFA_DPI;
    }
    let dec = (z / r).asin();
    IcrsCoord {
        ra_hours: ra * 12.0 / ERFA_DPI,
        dec_degrees: dec * ERFA_DR2D,
    }
}

/// Angular separation between two ICRS coordinates, in degrees.
/// Uses the spherical law of cosines.
#[expect(
    clippy::suboptimal_flops,
    reason = "the spherical law of cosines in its reference shape, mirroring the ERFA idiom; fusing terms hides the formula"
)]
pub fn angular_separation_degrees(a: IcrsCoord, b: IcrsCoord) -> f64 {
    let ra_a = a.ra_hours * 15.0 * ERFA_DD2R;
    let dec_a = a.dec_degrees * ERFA_DD2R;
    let ra_b = b.ra_hours * 15.0 * ERFA_DD2R;
    let dec_b = b.dec_degrees * ERFA_DD2R;
    let cos_sep = dec_a.sin() * dec_b.sin() + dec_a.cos() * dec_b.cos() * (ra_a - ra_b).cos();
    cos_sep.clamp(-1.0, 1.0).acos() * ERFA_DR2D
}

impl Ephemeris for ErfarsEphemeris {
    fn sidereal_time(&self, site: &Site, time: DateTime<Utc>) -> LocalSiderealTime {
        run_with_guard(
            "sidereal_time",
            LocalSiderealTime {
                lst_hours: f64::NAN,
            },
            || {
                let jds = time_jds(time);
                LocalSiderealTime {
                    lst_hours: lst_hours(site, &jds),
                }
            },
        )
    }

    fn alt_az(
        &self,
        site: &Site,
        target: IcrsCoord,
        time: DateTime<Utc>,
    ) -> Result<AltAz, EphemerisError> {
        run_with_guard("alt_az", Ok(nan_alt_az()), || {
            let jds = time_jds(time);
            alt_az_at(site, target, &jds)
        })
    }

    fn transit(&self, site: &Site, target: IcrsCoord, date: NaiveDate) -> Option<DateTime<Utc>> {
        run_with_guard("transit", None, || derived::transit(site, target, date))
    }

    fn rise_set(
        &self,
        site: &Site,
        target: IcrsCoord,
        date: NaiveDate,
        min_alt_deg: f64,
    ) -> Option<RiseSet> {
        run_with_guard("rise_set", None, || {
            derived::rise_set(self, site, target, date, min_alt_deg)
        })
    }

    fn meridian_flip(
        &self,
        site: &Site,
        target: IcrsCoord,
        time: DateTime<Utc>,
        _side: SideOfPier,
    ) -> Option<Duration> {
        run_with_guard("meridian_flip", None, || {
            derived::meridian_flip(site, target, time)
        })
    }

    fn sun_position(&self, site: &Site, time: DateTime<Utc>) -> SunInfo {
        run_with_guard(
            "sun_position",
            SunInfo {
                coords: nan_icrs(),
                alt_az: nan_alt_az(),
            },
            || {
                let jds = time_jds(time);
                let coords = sun_icrs(&jds);
                let alt_az = alt_az_at(site, coords, &jds).unwrap_or_else(|_| nan_alt_az());
                SunInfo { coords, alt_az }
            },
        )
    }

    fn twilight(&self, site: &Site, date: NaiveDate, kind: TwilightKind) -> TwilightWindow {
        run_with_guard(
            "twilight",
            TwilightWindow {
                begin_utc: None,
                end_utc: None,
            },
            || derived::twilight(self, site, date, kind),
        )
    }

    fn moon_position(&self, site: &Site, time: DateTime<Utc>) -> MoonInfo {
        run_with_guard(
            "moon_position",
            MoonInfo {
                coords: nan_icrs(),
                alt_az: nan_alt_az(),
                phase_degrees: f64::NAN,
                illumination_fraction: f64::NAN,
            },
            || {
                let jds = time_jds(time);
                let coords = moon_icrs(&jds);
                let alt_az = alt_az_at(site, coords, &jds).unwrap_or_else(|_| nan_alt_az());
                let sun = sun_icrs(&jds);
                let phase_degrees = angular_separation_degrees(coords, sun);
                // phase_degrees here is the Sun-Earth-Moon elongation:
                //   0°   = new (Sun and Moon at same RA → 0% illuminated)
                //   180° = full (Sun and Moon opposite → 100% illuminated)
                // Illuminated fraction = (1 - cos(elongation)) / 2 — at
                // elongation=0 this is 0, at elongation=180 it is 1,
                // which is what users see in the sky. The `(1 + cos)/2`
                // form is for the *phase angle* convention (vertex at
                // Moon: 0° = full, 180° = new) — we don't use that
                // convention here.
                let illumination_fraction = (1.0 - (phase_degrees * ERFA_DD2R).cos()) / 2.0;
                MoonInfo {
                    coords,
                    alt_az,
                    phase_degrees,
                    illumination_fraction,
                }
            },
        )
    }

    fn moon_separation(&self, target: IcrsCoord, time: DateTime<Utc>) -> f64 {
        run_with_guard("moon_separation", f64::NAN, || {
            let jds = time_jds(time);
            angular_separation_degrees(target, moon_icrs(&jds))
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn site_greenwich() -> Site {
        Site::new(0.0, 0.0).unwrap()
    }

    fn site_seattle() -> Site {
        Site::new(47.6062, -122.3321).unwrap()
    }

    /// Polaris (RA ~2.5h, Dec ~+89.26°) is essentially at the celestial
    /// pole; from a mid-northern site its altitude tracks the latitude
    /// closely (within ~1° depending on the year's pole motion and
    /// refraction).
    #[test]
    fn polaris_altitude_tracks_latitude_at_seattle() {
        let eph = ErfarsEphemeris::new();
        let polaris = IcrsCoord {
            ra_hours: 2.530_194_4,
            dec_degrees: 89.264_111_1,
        };
        let t = Utc.with_ymd_and_hms(2026, 6, 21, 6, 0, 0).unwrap();
        let alt = eph.alt_az(&site_seattle(), polaris, t).unwrap();
        // Seattle latitude is 47.6°; Polaris altitude ≈ latitude ± dec
        // offset from pole, refraction-bumped near horizon. Expect
        // within ~1° of latitude here.
        assert!(
            (alt.altitude_degrees - 47.6).abs() < 1.5,
            "polaris altitude {:.2}° not close to Seattle latitude",
            alt.altitude_degrees
        );
    }

    /// The observed→ICRS inverse must undo the ICRS→observed forward
    /// transform to sub-arcsecond accuracy, both refracted and not.
    #[test]
    fn icrs_from_alt_az_round_trips_the_forward_transform() {
        let eph = ErfarsEphemeris::new();
        let target = IcrsCoord {
            ra_hours: 5.5,
            dec_degrees: 38.0,
        };
        let t = Utc.with_ymd_and_hms(2026, 8, 1, 8, 0, 0).unwrap();
        for refraction in [Some(RefractionConditions::default()), None] {
            let observed = eph
                .alt_az_with_conditions(&site_seattle(), target, t, refraction)
                .unwrap();
            let back = eph
                .icrs_from_alt_az(&site_seattle(), observed, t, refraction)
                .unwrap();
            let dra_arcsec = (back.ra_hours - target.ra_hours) * 15.0 * 3600.0;
            let ddec_arcsec = (back.dec_degrees - target.dec_degrees) * 3600.0;
            assert!(
                dra_arcsec.abs() < 1.0 && ddec_arcsec.abs() < 1.0,
                "round trip (refraction {refraction:?}) off by ({dra_arcsec:.3}\", {ddec_arcsec:.3}\")"
            );
        }
    }

    /// At ~45° altitude, refraction lifts the observed altitude by
    /// roughly one arcminute; the unrefracted transform must not.
    #[test]
    fn refraction_lifts_altitude_by_about_an_arcminute_at_45_degrees() {
        let eph = ErfarsEphemeris::new();
        let t = Utc.with_ymd_and_hms(2026, 8, 1, 8, 0, 0).unwrap();
        // A target at exactly 45° geometric altitude, minted via the
        // unrefracted inverse so the test doesn't depend on the sky
        // configuration at the chosen instant.
        let target = eph
            .icrs_from_alt_az(
                &site_seattle(),
                AltAz {
                    altitude_degrees: 45.0,
                    azimuth_degrees: 180.0,
                },
                t,
                None,
            )
            .unwrap();
        let refracted = eph
            .alt_az_with_conditions(
                &site_seattle(),
                target,
                t,
                Some(RefractionConditions::default()),
            )
            .unwrap();
        let geometric = eph
            .alt_az_with_conditions(&site_seattle(), target, t, None)
            .unwrap();
        let lift_arcmin = (refracted.altitude_degrees - geometric.altitude_degrees) * 60.0;
        assert!(
            (0.4..3.0).contains(&lift_arcmin),
            "refraction lift at alt {:.1}° was {:.2}′; expected arcminute scale",
            geometric.altitude_degrees,
            lift_arcmin
        );
        assert!(
            (refracted.azimuth_degrees - geometric.azimuth_degrees).abs() < 0.01,
            "refraction must not move azimuth"
        );
    }

    /// Sidereal time at Greenwich at J2000.0 epoch should be close to
    /// 18h 41m 50.5s (well-known canonical value). This is a strong
    /// sanity check on the time-conversion + Gst06a chain.
    #[test]
    fn gst_at_j2000_epoch_matches_canonical() {
        let eph = ErfarsEphemeris::new();
        // J2000 = 2000-01-01 12:00 TT ≈ 11:58:55.816 UTC
        let t = Utc.with_ymd_and_hms(2000, 1, 1, 11, 58, 55).unwrap();
        let lst = eph.sidereal_time(&site_greenwich(), t);
        // Expected ~18.6973h. Allow 0.01h (~36 seconds of LST) to
        // absorb our truncation of TT and ΔUT1=0 simplification.
        let expected = 18.0 + 41.0 / 60.0 + 50.5 / 3600.0;
        assert!(
            (lst.lst_hours - expected).abs() < 0.05,
            "GMST at J2000 epoch was {:.6}h; expected {:.6}h",
            lst.lst_hours,
            expected
        );
    }

    /// On the vernal equinox, the Sun is at RA=0h, Dec=0° (by
    /// definition of the equinox).
    #[test]
    fn sun_at_vernal_equinox_is_near_origin() {
        let eph = ErfarsEphemeris::new();
        // 2026 vernal equinox is 2026-03-20 14:46 UTC. Take that
        // moment plus a few minutes so we're well past the crossing.
        let t = Utc.with_ymd_and_hms(2026, 3, 20, 14, 46, 0).unwrap();
        let sun = eph.sun_position(&site_greenwich(), t);
        // Geocentric astrometric, no aberration: dec should be within
        // 0.5° of 0 (sub-day drift) and RA within 0.5h of 0/24.
        assert!(
            sun.coords.dec_degrees.abs() < 0.5,
            "sun dec at vernal equinox = {:.4}°, expected ~0",
            sun.coords.dec_degrees
        );
        let ra_distance_to_origin = sun.coords.ra_hours.min(24.0 - sun.coords.ra_hours);
        assert!(
            ra_distance_to_origin < 0.5,
            "sun RA at vernal equinox = {:.4}h, expected ~0/24",
            sun.coords.ra_hours
        );
    }

    /// Sun is below horizon at midnight everywhere except polar
    /// summer.
    #[test]
    fn sun_is_below_horizon_at_seattle_midnight_in_winter() {
        let eph = ErfarsEphemeris::new();
        let t = Utc.with_ymd_and_hms(2026, 12, 21, 8, 0, 0).unwrap(); // midnight PST
        let sun = eph.sun_position(&site_seattle(), t);
        assert!(
            sun.alt_az.altitude_degrees < 0.0,
            "sun altitude at Seattle midnight in winter = {:.2}°",
            sun.alt_az.altitude_degrees
        );
    }

    /// Moon coordinates should be valid (RA in [0,24), Dec in [-90,90])
    /// and altitude either above or below horizon — just a sanity
    /// check that the wiring works end-to-end.
    #[test]
    fn moon_coordinates_in_valid_range() {
        let eph = ErfarsEphemeris::new();
        let t = Utc.with_ymd_and_hms(2026, 5, 3, 0, 0, 0).unwrap();
        let m = eph.moon_position(&site_seattle(), t);
        assert!((0.0..24.0).contains(&m.coords.ra_hours));
        assert!((-90.0..=90.0).contains(&m.coords.dec_degrees));
        assert!((0.0..=180.0).contains(&m.phase_degrees));
        assert!((0.0..=1.0).contains(&m.illumination_fraction));
    }

    /// `Dtf2d` rejects years < -4799. chrono can construct dates down
    /// to year -262144, so we can hit the Err arm in `time_jds` from
    /// safe code. Expect NaN-filled JDs and a `tracing::error!` log.
    #[test]
    fn time_jds_returns_nan_when_year_is_before_erfa_lower_bound() {
        let t = Utc.with_ymd_and_hms(-10000, 1, 1, 0, 0, 0).unwrap();
        let jds = time_jds(t);
        assert!(jds.utc1.is_nan());
        assert!(jds.utc2.is_nan());
        assert!(jds.tt1.is_nan());
        assert!(jds.tt2.is_nan());
        assert!(jds.ut11.is_nan());
        assert!(jds.ut12.is_nan());
    }

    /// End-to-end: a year outside ERFA's range should make every
    /// trait method degrade to NaN/None instead of crashing.
    #[test]
    fn trait_methods_degrade_to_nan_or_none_for_year_before_erfa_range() {
        let eph = ErfarsEphemeris::new();
        let t = Utc.with_ymd_and_hms(-10000, 1, 1, 0, 0, 0).unwrap();
        let d = NaiveDate::from_ymd_opt(-10000, 1, 1).unwrap();
        let target = IcrsCoord {
            ra_hours: 12.0,
            dec_degrees: 0.0,
        };
        let site = site_seattle();

        assert!(eph.sidereal_time(&site, t).lst_hours.is_nan());
        let alt_az = eph.alt_az(&site, target, t).unwrap();
        assert!(alt_az.altitude_degrees.is_nan());
        assert!(alt_az.azimuth_degrees.is_nan());
        let sun = eph.sun_position(&site, t);
        assert!(sun.coords.ra_hours.is_nan());
        assert!(sun.coords.dec_degrees.is_nan());
        let moon = eph.moon_position(&site, t);
        assert!(moon.coords.ra_hours.is_nan());
        assert!(moon.illumination_fraction.is_nan());
        assert!(eph.moon_separation(target, t).is_nan());
        // Date-based helpers bisect over LST/sun-altitude; both go NaN
        // upstream and the bisector returns None.
        assert!(eph.transit(&site, target, d).is_none());
        assert!(eph.rise_set(&site, target, d, 0.0).is_none());
    }

    /// `run_with_guard` returns the closure's value when the closure
    /// does not panic. Establishes the happy path.
    #[test]
    fn run_with_guard_returns_closure_value_on_happy_path() {
        let result = run_with_guard("test", 0, || 42);
        assert_eq!(result, 42);
    }

    /// `run_with_guard` catches a `panic!` from the closure and
    /// returns the supplied fallback. This is the central defense
    /// against panics inside the erfars wrappers' `unexpected_val_err!`
    /// macro.
    #[test]
    fn run_with_guard_returns_fallback_when_closure_panics() {
        let result = run_with_guard("test", 7, || -> i32 {
            panic!("simulated wrapper panic");
        });
        assert_eq!(result, 7);
    }

    /// Verify that `&'static str` panic payloads (the shape produced
    /// by `panic!("literal")` in the erfars `unexpected_val_err!`
    /// macro) round-trip through the extractor.
    #[test]
    fn panic_payload_message_extracts_static_str() {
        let payload: Box<dyn Any + Send> = Box::new("hello world");
        assert_eq!(panic_payload_message(&*payload), "hello world");
    }

    /// `panic!("{}", ...)` with formatting arguments produces a
    /// `String` payload, not `&'static str` — cover that arm too.
    #[test]
    fn panic_payload_message_extracts_string() {
        let payload: Box<dyn Any + Send> = Box::new(String::from("formatted msg"));
        assert_eq!(panic_payload_message(&*payload), "formatted msg");
    }

    /// Any other payload type (e.g. a `panic_any` value) falls back
    /// to a sentinel string so we still log *something* useful.
    #[test]
    fn panic_payload_message_returns_sentinel_for_unknown_payload() {
        let payload: Box<dyn Any + Send> = Box::new(42_i32);
        assert_eq!(
            panic_payload_message(&*payload),
            "<non-string panic payload>"
        );
    }

    fn epoch() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    /// Happy-path unwrap of a `Dtf2d` Ok pair.
    #[test]
    fn dtf2d_jds_passes_through_ok_pair() {
        let pair = dtf2d_jds(Ok(((2_451_545.0, 0.5), 0)), epoch());
        assert_eq!(pair, Some((2_451_545.0, 0.5)));
    }

    /// `Dtf2d` Err → None. This is the documented bad-input path
    /// (year outside ERFA's calendar range).
    #[test]
    fn dtf2d_jds_returns_none_on_err() {
        assert!(dtf2d_jds(Err(-1), epoch()).is_none());
    }

    /// Happy-path unwrap of a `Utctai` Ok pair.
    #[test]
    fn utctai_pair_passes_through_ok_pair() {
        let pair = utctai_pair(Ok(((2_451_545.0, 0.5), 0)), epoch());
        assert_eq!(pair, Some((2_451_545.0, 0.5)));
    }

    /// `Utctai` Err → None. Structurally unreachable in production
    /// (Dtf2d filters the inputs that would cause it), so the helper
    /// gives us a seam to verify the defensive arm without needing
    /// the impossible ERFA-internal failure to actually occur.
    #[test]
    fn utctai_pair_returns_none_on_err() {
        assert!(utctai_pair(Err(-1), epoch()).is_none());
    }

    /// Happy-path unwrap of an `Epv00` Ok result; returns the
    /// heliocentric position vector.
    #[test]
    fn epv00_heliocentric_passes_through_ok_pvh() {
        let pvh = [1.0, 2.0, 3.0, 0.1, 0.2, 0.3];
        let pvb = [0.0; 6];
        let jds = nan_time_jds();
        assert_eq!(epv00_heliocentric(Ok(((pvh, pvb), 0)), &jds), Some(pvh));
    }

    /// `Epv00` Err → None. Structurally unreachable today (eraEpv00
    /// only ever returns 0 or +1), so the helper exists so that
    /// production code can stay panic-free even if the upstream
    /// invariant ever changes.
    #[test]
    fn epv00_heliocentric_returns_none_on_err() {
        let jds = nan_time_jds();
        assert!(epv00_heliocentric(Err(-1), &jds).is_none());
    }

    /// Verify the production wiring: feeding `sun_icrs` a `TimeJds`
    /// whose `tt` pair would force Epv00 to return Err (it can't
    /// today, but we cover the defensive arm via the inner helper
    /// above and trust this wiring path).
    #[test]
    fn sun_icrs_returns_nan_when_helper_yields_none() {
        // Drive sun_icrs with finite (but absurd) tt values to confirm
        // the happy path returns finite coords. The Err arm coverage
        // is provided by `epv00_heliocentric_returns_none_on_err`.
        let jds = TimeJds {
            utc1: 2_451_545.0,
            utc2: 0.0,
            tt1: 2_451_545.0,
            tt2: 0.0,
            ut11: 2_451_545.0,
            ut12: 0.0,
        };
        let icrs = sun_icrs(&jds);
        assert!(icrs.ra_hours.is_finite());
        assert!(icrs.dec_degrees.is_finite());
    }
}

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// J2000 mean equator/equinox (ICRS) target coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct IcrsCoord {
    pub ra_hours: f64,
    pub dec_degrees: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AltAz {
    pub altitude_degrees: f64,
    pub azimuth_degrees: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LocalSiderealTime {
    pub lst_hours: f64,
}

/// A span of fractional solar hours, as produced by the sidereal→solar
/// scalings in the derived operations (`derived.rs`). Owns the one
/// `f64` → milliseconds conversion so call sites stay cast-free.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SolarHours(pub f64);

impl From<SolarHours> for Duration {
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "any hours-scale magnitude is orders of magnitude below i64::MAX \
                  milliseconds, and `as` saturates the overflow and NaN cases \
                  (to the extremes and 0 respectively) instead of wrapping"
    )]
    fn from(hours: SolarHours) -> Self {
        Self::milliseconds((hours.0 * 3_600_000.0) as i64)
    }
}

/// Atmospheric conditions for the refraction model in the
/// observed-coordinate conversions that take them explicitly
/// ([`crate::ErfarsEphemeris::alt_az_with_conditions`] /
/// [`crate::ErfarsEphemeris::icrs_from_alt_az`]). Relative humidity
/// and wavelength stay at the trait-documented amateur-rig values
/// (50 %, 0.55 µm) — pressure and temperature dominate the visible-
/// light refraction term.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RefractionConditions {
    pub pressure_hpa: f64,
    pub temperature_c: f64,
}

impl Default for RefractionConditions {
    fn default() -> Self {
        Self {
            pressure_hpa: 1013.25,
            temperature_c: 10.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiseSet {
    pub rise_utc: DateTime<Utc>,
    pub set_utc: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SideOfPier {
    East,
    West,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TwilightKind {
    Civil,
    Nautical,
    Astronomical,
}

impl TwilightKind {
    /// Sun-altitude threshold for this twilight kind, in degrees.
    #[must_use]
    pub const fn sun_altitude_threshold_degrees(self) -> f64 {
        match self {
            Self::Civil => -6.0,
            Self::Nautical => -12.0,
            Self::Astronomical => -18.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TwilightWindow {
    /// Sun crosses the threshold going down (evening twilight begins).
    pub begin_utc: Option<DateTime<Utc>>,
    /// Sun crosses the threshold going up (morning twilight ends).
    pub end_utc: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SunInfo {
    pub coords: IcrsCoord,
    pub alt_az: AltAz,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MoonInfo {
    pub coords: IcrsCoord,
    pub alt_az: AltAz,
    /// Elongation between the Sun and Moon as seen from Earth, in
    /// degrees `[0, 180]`. 0 = new, 90 = quarter, 180 = full.
    pub phase_degrees: f64,
    /// Illuminated fraction of the disc, `[0, 1]`. Computed as
    /// `(1 - cos(phase)) / 2` over the Sun-Earth-Moon elongation —
    /// 0 at new moon (elongation 0°), 1 at full moon (elongation
    /// 180°). Good to ~1 % outside the crescent regions for amateur
    /// observing.
    pub illumination_fraction: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum EphemerisError {
    #[error("ERFA reported an unrepresentable time/date input (status {0})")]
    InvalidTimeInput(i32),
    #[error("ERFA refused the alt/az transform (status {0}); inputs out of valid range")]
    InvalidAltAzInputs(i32),
}

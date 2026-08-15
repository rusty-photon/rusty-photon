#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! Astronomical math for Rusty Photon.
//!
//! [`Ephemeris`] is the seam between the math layer and everything that
//! consumes positions, times, and twilight windows. The shipped impl
//! [`ErfarsEphemeris`] wraps the [`erfars`] crate (Rust FFI for ERFA, the
//! BSD-licensed clean-room derivative of IAU SOFA used by Astropy).
//!
//! All trait methods are pure functions. The trait surface contains zero
//! `unsafe` and no FFI types.

#![deny(unsafe_code)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
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
        clippy::struct_excessive_bools,
    )
)]

mod derived;
mod erfars_impl;
mod site;
mod types;
mod vocabulary;

pub use erfars_impl::ErfarsEphemeris;
pub use site::{Site, SiteError};
pub use types::{
    AltAz, EphemerisError, IcrsCoord, LocalSiderealTime, MoonInfo, RefractionConditions, RiseSet,
    SideOfPier, SunInfo, TwilightKind, TwilightWindow,
};

use chrono::{DateTime, Duration, NaiveDate, Utc};

/// Pure-function astronomical math.
///
/// All methods accept inputs by value and return owned values; no
/// implementation is allowed to retain mutable state across calls (the
/// `&self` is for caching only, never for state machines).
pub trait Ephemeris {
    /// Local apparent sidereal time at `site` for `time`, in hours
    /// `[0, 24)`. ΔUT1 is treated as zero (UT1 ≈ UTC, error ≤ 0.9 s
    /// = ≤ 13″ of LST).
    fn sidereal_time(&self, site: &Site, time: DateTime<Utc>) -> LocalSiderealTime;

    /// Topocentric altitude/azimuth of an ICRS target. Refraction is
    /// modelled with default amateur-rig conditions (1013.25 mb, 10 °C,
    /// 50 % RH, 0.55 µm).
    fn alt_az(
        &self,
        site: &Site,
        target: IcrsCoord,
        time: DateTime<Utc>,
    ) -> Result<AltAz, EphemerisError>;

    /// UTC time of upper transit on the given UTC `date`, or `None` if
    /// the target is circumpolar without ever crossing the meridian on
    /// that date (i.e. the south circumpolar limit of the southern
    /// hemisphere). For practical observing latitudes this returns
    /// `Some` for every target every day.
    fn transit(&self, site: &Site, target: IcrsCoord, date: NaiveDate) -> Option<DateTime<Utc>>;

    /// Rise and set times above `min_alt_deg` on the given UTC date.
    /// `None` if the target never reaches `min_alt_deg` (always-down
    /// circumpolar) or never falls below it (always-up circumpolar).
    fn rise_set(
        &self,
        site: &Site,
        target: IcrsCoord,
        date: NaiveDate,
        min_alt_deg: f64,
    ) -> Option<RiseSet>;

    /// Time until the target next crosses the meridian (HA = 0). Side
    /// of pier is read but not currently consulted — the v1
    /// implementation returns the next meridian crossing in the
    /// future regardless of the mount's current pier side.
    fn meridian_flip(
        &self,
        site: &Site,
        target: IcrsCoord,
        time: DateTime<Utc>,
        side: SideOfPier,
    ) -> Option<Duration>;

    /// Geocentric astrometric position of the Sun, plus topocentric
    /// alt/az at `site`. Annual aberration is not applied (sub-arcmin
    /// effect; below the resolution that matters for "is the Sun
    /// up?").
    fn sun_position(&self, site: &Site, time: DateTime<Utc>) -> SunInfo;

    /// Civil/nautical/astronomical twilight window centred on the
    /// local night that covers `date` (UTC). Either bound is `None`
    /// if the Sun never crosses the threshold altitude (polar day
    /// or polar night).
    fn twilight(&self, site: &Site, date: NaiveDate, kind: TwilightKind) -> TwilightWindow;

    /// Geocentric Moon position, topocentric alt/az, plus phase
    /// (Sun-Earth-Moon elongation, 0–180°) and illumination fraction.
    fn moon_position(&self, site: &Site, time: DateTime<Utc>) -> MoonInfo;

    /// Angular separation between an ICRS target and the Moon, in
    /// degrees. Geocentric — does not depend on `site`.
    fn moon_separation(&self, target: IcrsCoord, time: DateTime<Utc>) -> f64;
}

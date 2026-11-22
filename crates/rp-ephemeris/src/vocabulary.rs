//! Boundary conversions between this crate's computed [`IcrsCoord`] and the
//! validated plan coordinate [`rp_vocabulary::IcrsCoord`].
//!
//! These are two deliberately *different* types. `rp_vocabulary::IcrsCoord`
//! is a **plan value**: private fields, `try_new`-validated to
//! `ra_hours ∈ [0, 24)` / `dec_degrees ∈ [-90, 90]`, so an invalid pointing
//! is unrepresentable. This crate's [`IcrsCoord`] is a **computed
//! astronomy value**: `Epv00`/`Moon98`/`cartesian_to_icrs` build it from
//! ERFA output, and the degradation contract fills it with `NaN` when the
//! host clock or an upstream wrapper misbehaves (`sun_position`/
//! `moon_position` return NaN-filled coords rather than panicking — see
//! [rp-ephemeris.md § Panic Safety and Degradation](../../../docs/crates/rp-ephemeris.md#panic-safety-and-degradation)).
//! A validated newtype cannot serve that role: `try_new` rejects `NaN`, and
//! its half-open `[0, 24)` upper bound rejects a body computed to land
//! exactly on the RA=24h≡0h seam.
//!
//! Hence the asymmetry these impls encode:
//!
//! * **plan → computed is total** ([`From`]): a validated coordinate is
//!   always a well-formed input to the transforms.
//! * **computed → plan is partial** ([`TryFrom`]): a computed coordinate may
//!   be `NaN` (the degradation sentinel) or sit on the `24.0h` seam, so the
//!   conversion can fail. A plain `From` here would have to `unwrap` (a
//!   position-triggered panic in safety-adjacent code) or clamp (silent
//!   wrong data); `TryFrom` surfaces the seam as a [`CoordError`] the caller
//!   handles explicitly.

use rp_vocabulary::{CoordError, IcrsCoord as PlanCoord};

use crate::types::IcrsCoord;

/// Validated plan coordinate → computed coordinate. Total: a
/// `PlanCoord` is valid by construction, so this never fails.
impl From<PlanCoord> for IcrsCoord {
    fn from(c: PlanCoord) -> Self {
        Self {
            ra_hours: c.ra_hours(),
            dec_degrees: c.dec_degrees(),
        }
    }
}

/// Computed coordinate → validated plan coordinate. Partial: a computed
/// value can be `NaN` (the sun/moon degradation sentinel) or land exactly
/// on the `ra_hours == 24.0` seam, either of which `try_new` rejects.
impl TryFrom<IcrsCoord> for PlanCoord {
    type Error = CoordError;

    fn try_from(c: IcrsCoord) -> Result<Self, Self::Error> {
        Self::try_new(c.ra_hours, c.dec_degrees)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn plan_to_computed_preserves_values() {
        let plan = PlanCoord::try_new(6.5, -23.25).unwrap();
        let computed = IcrsCoord::from(plan);
        assert_eq!(computed.ra_hours, 6.5);
        assert_eq!(computed.dec_degrees, -23.25);
    }

    #[test]
    fn computed_to_plan_round_trips_a_valid_value() {
        let computed = IcrsCoord {
            ra_hours: 6.5,
            dec_degrees: -23.25,
        };
        let plan = PlanCoord::try_from(computed).unwrap();
        assert_eq!(plan.ra_hours(), 6.5);
        assert_eq!(plan.dec_degrees(), -23.25);
    }

    #[test]
    fn computed_to_plan_rejects_the_nan_degradation_sentinel() {
        // sun_position/moon_position hand back NaN-filled coords when ERFA
        // rejects the host clock; that sentinel is not a valid plan value.
        let sentinel = IcrsCoord {
            ra_hours: f64::NAN,
            dec_degrees: f64::NAN,
        };
        assert!(matches!(
            PlanCoord::try_from(sentinel),
            Err(CoordError::RaOutOfRange { .. })
        ));
    }

    #[test]
    fn computed_to_plan_rejects_the_ra_24h_seam() {
        // A body computed to sit on RA 0 can normalise to exactly 24.0h;
        // the half-open [0, 24) invariant rejects it rather than aliasing.
        let seam = IcrsCoord {
            ra_hours: 24.0,
            dec_degrees: 0.0,
        };
        assert!(matches!(
            PlanCoord::try_from(seam),
            Err(CoordError::RaOutOfRange { .. })
        ));
    }
}

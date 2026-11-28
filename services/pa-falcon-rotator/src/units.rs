//! Typed angular and step quantities for the Falcon Rotator.
//!
//! The Falcon exposes a single physical angle. ASCOM's `IRotatorV3+` splits
//! that into a **mechanical** frame (the device's own zero) and a **sky**
//! frame (the client's coordinate), joined by a driver-side [`SyncOffset`] —
//! see the design doc's
//! [ASCOM Rotator Mapping](../../../docs/services/falcon-rotator.md#ascom-rotator-mapping)
//! and
//! [Sync semantics](../../../docs/services/falcon-rotator.md#sync-semantics--why-driver-side-not-sd)
//! sections. These newtypes make the frame explicit at the type level so the
//! offset arithmetic can't be written wrong: you cannot add two sky angles,
//! or cross between frames without applying the offset, without a compile
//! error.
//!
//! The whole algebra is four operations, mirroring the four relations the
//! driver computes:
//!
//! ```text
//! Position       mech + offset = sky     MechanicalDegrees + SyncOffset -> SkyDegrees
//! MoveAbsolute   sky  - offset = mech    SkyDegrees - SyncOffset        -> MechanicalDegrees
//! Sync           sky  - mech   = offset  SkyDegrees - MechanicalDegrees -> SyncOffset
//! Move(delta)    mech.rotate(delta)      relative rotation, stays mechanical
//! ```
//!
//! [`Steps`] is the Falcon's signed encoder count (negative CCW of the 0°
//! home). The driver derives every position from the degree field, so on the
//! wire `Steps` is informational; it earns its keep in the mock, which models
//! the device's signed counter via the [`STEPS_PER_DEGREE`] conversion.

use std::ops::{Add, Sub};

/// Steps per degree (vendor product page). The mock converts between its
/// signed step counter and a mechanical angle with this; the driver itself
/// works in degrees and never converts.
pub const STEPS_PER_DEGREE: f64 = 86.6;

/// Normalise a degree value into `[0.0, 360.0)`.
///
/// An angle already in range is kept as it is, except that a negative zero
/// becomes plain zero (`-0.0 + 0.0` is `0.0`; every other value is
/// unchanged by the `+ 0.0`), so the wire never carries `MD:-0.00`. Only the
/// rest go through the standard `((x % 360) + 360) % 360`, which wraps
/// negative inputs (a `Move(delta < 0)`, or a `sky - mech` that goes
/// negative) and sums past 360 into the canonical range: adding 360 and
/// taking it off again is not exact in binary floating point, and would turn
/// the device's `147.84` into `147.84000000000003`. Every constructor below
/// funnels through this, so each angle type holds a normalised value by
/// construction.
fn normalise_deg(deg: f64) -> f64 {
    if (0.0..360.0).contains(&deg) {
        deg + 0.0
    } else {
        ((deg % 360.0) + 360.0) % 360.0
    }
}

/// Round an angle in `[0, 360)` to the Falcon's 1/100°, the precision of
/// the `FA` degree field and of the `MD:nn.nn` target: the result is the
/// double nearest the two-decimal value. One that rounds up to `360.00` is
/// `0.00`, so the result stays in range.
fn round_to_hundredths(deg: f64) -> f64 {
    let rounded = (deg * 100.0).round() / 100.0;
    if rounded >= 360.0 {
        0.0
    } else {
        rounded
    }
}

/// A mechanical angle in `[0, 360)` — the Falcon's own frame: the `FA`/`FD`
/// degree field, the `MD:` wire target, and ASCOM `MechanicalPosition`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MechanicalDegrees(f64);

/// A sky angle in `[0, 360)` — the ASCOM client's frame: `Position`, the
/// `MoveAbsolute` / `Sync` targets, and `TargetPosition`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkyDegrees(f64);

/// The driver-side `Sync` offset (`sky - mech`), in `[0, 360)` and held at
/// the device's 1/100°. Bridges the two frames: `mech + offset = sky` and
/// `sky - offset = mech`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SyncOffset(f64);

impl MechanicalDegrees {
    /// Construct from a degree value, normalising into `[0, 360)`.
    ///
    /// Callers must reject non-finite input first (the ASCOM boundary and the
    /// wire parsers both do); a non-finite value would propagate as `NaN`.
    #[must_use]
    pub fn new(deg: f64) -> Self {
        Self(normalise_deg(deg))
    }

    /// The underlying degree value in `[0, 360)`.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.0
    }

    /// Quantise to the `MD:nn.nn` wire precision (1/100°); a near-boundary
    /// value like `359.999` becomes `0.00` rather than the out-of-range
    /// `360.00`.
    #[must_use]
    pub fn quantise_to_wire(self) -> Self {
        Self(round_to_hundredths(self.0))
    }

    /// Apply a relative rotation (degrees, may be negative), staying in the
    /// mechanical frame. Models the target of ASCOM `Move(delta)`.
    #[must_use]
    pub fn rotate(self, delta: f64) -> Self {
        Self::new(self.0 + delta)
    }
}

impl SkyDegrees {
    /// Construct from a degree value, normalising into `[0, 360)`. See
    /// [`MechanicalDegrees::new`] for the non-finite precondition.
    #[must_use]
    pub fn new(deg: f64) -> Self {
        Self(normalise_deg(deg))
    }

    /// The underlying degree value in `[0, 360)`.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.0
    }
}

impl SyncOffset {
    /// A zero offset: the unsynced state, before any `Sync`.
    pub const ZERO: Self = Self(0.0);

    /// Construct from a degree value, normalising into `[0, 360)` and
    /// rounding to 1/100°. See [`MechanicalDegrees::new`] for the
    /// non-finite precondition.
    ///
    /// The mechanical angle the device reports is two-decimal, so with a
    /// two-decimal offset every `mech + offset` and `sky - offset` is
    /// two-decimal too, up to binary residue far below a hundredth. Each
    /// then rounds back to the value it came from, and `MoveAbsolute` to
    /// the `Position` read commands the mechanical angle the rotator is
    /// at. An offset with finer digits rounds twice on that round trip,
    /// and one whose sub-hundredth part is a tie at 0.005 (a `Sync` to
    /// `112.345` at `100.00`) moves the rotator 0.01° and reads back
    /// 0.01° off. A `Sync` and the record's load both construct here.
    #[must_use]
    pub fn new(deg: f64) -> Self {
        Self(round_to_hundredths(normalise_deg(deg)))
    }

    /// The underlying offset value in `[0, 360)`.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.0
    }
}

/// `mech + offset = sky` — lift a mechanical angle into the sky frame, at
/// the device's 1/100°. The sum of two decimal fractions is rarely exact in
/// binary, and the offset can carry finer digits than the device resolves
/// (a `Sync` to an arbitrary angle), so the sky angle is rounded to the
/// precision the mechanical one has. This is what `Position` and
/// `TargetPosition` report.
impl Add<SyncOffset> for MechanicalDegrees {
    type Output = SkyDegrees;

    fn add(self, offset: SyncOffset) -> SkyDegrees {
        SkyDegrees(round_to_hundredths(normalise_deg(self.0 + offset.0)))
    }
}

/// `sky - offset = mech` — drop a sky angle into the mechanical frame.
impl Sub<SyncOffset> for SkyDegrees {
    type Output = MechanicalDegrees;

    fn sub(self, offset: SyncOffset) -> MechanicalDegrees {
        MechanicalDegrees::new(self.0 - offset.0)
    }
}

/// `sky - mech = offset` — the `Sync` operation: the offset that makes the
/// current mechanical angle read as `sky`.
impl Sub<MechanicalDegrees> for SkyDegrees {
    type Output = SyncOffset;

    fn sub(self, mech: MechanicalDegrees) -> SyncOffset {
        SyncOffset::new(self.0 - mech.0)
    }
}

/// The Falcon's signed step counter relative to the 0° home (negative CCW of
/// home), as reported in the `FA` step field and by `FP`.
///
/// Informational on the wire — the driver derives all positions from the
/// degree field — but the mock models the device's counter with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Steps(pub i32);

impl Steps {
    /// The underlying signed step count.
    #[must_use]
    pub const fn value(self) -> i32 {
        self.0
    }
}

/// A step count maps to exactly one mechanical angle (`steps / 86.6`,
/// normalised). The reverse direction is device-specific — the past-limit
/// sign convention lives in the mock — so it is deliberately not a `From`.
impl From<Steps> for MechanicalDegrees {
    fn from(steps: Steps) -> Self {
        Self::new(f64::from(steps.0) / STEPS_PER_DEGREE)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    const EPS: f64 = 1e-9;

    // ---- Construction / normalisation ------------------------------------

    #[test]
    fn mechanical_new_passes_through_in_range() {
        assert!((MechanicalDegrees::new(142.30).value() - 142.30).abs() < EPS);
    }

    #[test]
    fn mechanical_new_wraps_positive_overflow() {
        assert!((MechanicalDegrees::new(370.0).value() - 10.0).abs() < EPS);
    }

    #[test]
    fn mechanical_new_wraps_negative_into_positive() {
        assert!((MechanicalDegrees::new(-10.0).value() - 350.0).abs() < EPS);
    }

    #[test]
    fn sky_new_wraps() {
        assert!((SkyDegrees::new(720.5).value() - 0.5).abs() < EPS);
    }

    #[test]
    fn offset_new_wraps_negative() {
        // 37.5 - 142.3 = -104.8 → +360 = 255.2
        assert!((SyncOffset::new(-104.8).value() - 255.2).abs() < EPS);
    }

    #[test]
    fn an_angle_in_range_is_kept_exactly() {
        // The full wrap, `((x % 360) + 360) % 360`, perturbs each of these:
        // it turns 147.84 into 147.84000000000003.
        for deg in [147.84, 12.3, 0.01, 200.07, 359.99, 0.0] {
            assert_eq!(MechanicalDegrees::new(deg).value(), deg);
            assert_eq!(SkyDegrees::new(deg).value(), deg);
            assert_eq!(SyncOffset::new(deg).value(), deg);
        }
    }

    #[test]
    fn negative_zero_is_plain_zero_in_every_frame() {
        assert!(MechanicalDegrees::new(-0.0).value().is_sign_positive());
        assert!(SkyDegrees::new(-0.0).value().is_sign_positive());
        assert!(SyncOffset::new(-0.0).value().is_sign_positive());
        // `MoveAbsolute(-0.0)` with no offset: the wire target is `MD:0.00`.
        let wire = (SkyDegrees::new(-0.0) - SyncOffset::ZERO).quantise_to_wire();
        assert!(wire.value().is_sign_positive());
    }

    #[test]
    fn a_sync_offset_is_held_to_a_hundredth() {
        // 37.50 - 142.30 is 255.20000000000005 in binary.
        let offset = SkyDegrees::new(37.50) - MechanicalDegrees::new(142.30);
        assert_eq!(offset.value(), 255.2);
        assert_eq!(SyncOffset::new(12.345).value(), 12.35);
    }

    #[test]
    fn a_move_to_the_position_read_after_a_sync_commands_where_the_rotator_is() {
        // Each sync leaves a sub-hundredth part of the offset at a 0.005 tie.
        for (mech, sync) in [(100.00, 112.345), (12.30, 0.005), (200.07, 100.015)] {
            let mech = MechanicalDegrees::new(mech);
            let offset = SkyDegrees::new(sync) - mech;
            let position = mech + offset;
            let wire = (position - offset).quantise_to_wire();
            assert_eq!(wire.value(), mech.value());
            assert_eq!((wire + offset).value(), position.value());
        }
    }

    #[test]
    fn offset_zero_is_zero() {
        assert!((SyncOffset::ZERO.value()).abs() < EPS);
        assert_eq!(SyncOffset::default(), SyncOffset::ZERO);
    }

    // ---- quantise_to_wire ------------------------------------------------

    #[test]
    fn quantise_rounds_to_two_decimals() {
        // 142.29792… (the integer-step round-trip of 142.30) → 142.30
        assert!(
            (MechanicalDegrees::new(142.297_921)
                .quantise_to_wire()
                .value()
                - 142.30)
                .abs()
                < EPS
        );
    }

    #[test]
    fn quantise_lands_exactly_on_the_two_decimal_value() {
        assert_eq!(
            MechanicalDegrees::new(102.840_000_000_003)
                .quantise_to_wire()
                .value(),
            102.84
        );
    }

    #[test]
    fn quantise_wraps_boundary_to_zero() {
        // 359.999 would format as 360.00 (out of range); quantise + normalise
        // yields 0.00 instead.
        assert!(MechanicalDegrees::new(359.999).quantise_to_wire().value() < EPS);
    }

    // ---- rotate ----------------------------------------------------------

    #[test]
    fn rotate_positive_wraps() {
        assert!((MechanicalDegrees::new(350.0).rotate(20.0).value() - 10.0).abs() < EPS);
    }

    #[test]
    fn rotate_negative_wraps() {
        assert!((MechanicalDegrees::new(10.0).rotate(-30.0).value() - 340.0).abs() < EPS);
    }

    // ---- Frame algebra ---------------------------------------------------

    #[test]
    fn mech_plus_offset_is_sky() {
        // 142.30 + 255.20 = 397.50 → 37.50
        let sky = MechanicalDegrees::new(142.30) + SyncOffset::new(255.20);
        assert!((sky.value() - 37.50).abs() < EPS);
    }

    #[test]
    fn mech_plus_offset_is_the_sky_angle_to_a_hundredth_exactly() {
        // Unrounded, these sums read 37.50000000000006, 172.97000000000003
        // and, past 360 and wrapped, 187.91000000000008.
        for (mech, sky) in [(142.30, 37.50), (147.84, 172.97), (200.07, 187.91)] {
            let mech = MechanicalDegrees::new(mech);
            let offset = SkyDegrees::new(sky) - mech;
            assert_eq!((mech + offset).value(), sky);
        }
    }

    #[test]
    fn mech_plus_offset_rounding_up_to_360_is_zero() {
        let sky = MechanicalDegrees::new(359.99) + SyncOffset::new(0.006);
        assert_eq!(sky.value(), 0.0);
    }

    #[test]
    fn sky_minus_offset_is_mech() {
        // 180.00 - 255.20 = -75.20 → 284.80
        let mech = SkyDegrees::new(180.0) - SyncOffset::new(255.20);
        assert!((mech.value() - 284.80).abs() < EPS);
    }

    #[test]
    fn sky_minus_mech_is_offset() {
        // 37.50 - 142.30 = -104.80 → 255.20
        let offset = SkyDegrees::new(37.50) - MechanicalDegrees::new(142.30);
        assert!((offset.value() - 255.20).abs() < EPS);
    }

    #[test]
    fn lifting_then_dropping_round_trips() {
        let mech = MechanicalDegrees::new(142.30);
        let offset = SyncOffset::new(255.20);
        let back = (mech + offset) - offset;
        assert!((back.value() - mech.value()).abs() < EPS);
    }

    // ---- Steps ↔ degrees -------------------------------------------------

    #[test]
    fn steps_to_mechanical_positive() {
        // 50° * 86.6 = 4330 steps; 4330 / 86.6 = 50°
        assert!((MechanicalDegrees::from(Steps(4330)).value() - 50.0).abs() < EPS);
    }

    #[test]
    fn negative_steps_below_home_wrap_into_range() {
        // -5196 / 86.6 = -60° → 300° (a target past the 220° CW limit).
        assert!((MechanicalDegrees::from(Steps(-5196)).value() - 300.0).abs() < EPS);
    }

    #[test]
    fn steps_value_round_trips() {
        assert_eq!(Steps(-2838).value(), -2838);
    }
}

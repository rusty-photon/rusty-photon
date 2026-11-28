//! Temperature differences compared to 0.001 °C (rp.md § Temperature
//! comparisons).
//!
//! Every threshold rp holds a temperature against — the cooling
//! tolerance, plateau threshold and regulation margin, the temperature
//! watch's event delta — is a decimal, and so are the readings: a ZWO
//! camera reports tenths of a degree, a ZWO EAF an `f32` widened to
//! `f64`. Their binary difference misses the decimal one by a residue:
//! −19.7 − (−20) computes as 0.3000000000000007 and 16.4 − 15.9 as
//! 0.4999999999999982, so a bare `f64` comparison puts a reading of
//! exactly the threshold on whichever side the residue happens to fall.
//! [`TemperatureDelta`] rounds both the difference and the threshold to
//! whole thousandths of a degree — far below any sensor's resolution, far
//! above `f32` and `f64` residue — and orders those exactly.

/// Thousandths of a degree in one degree Celsius.
const MILLIDEGREES_PER_DEGREE: f64 = 1000.0;

/// A difference between two temperatures, or a threshold one is held
/// against, in whole thousandths of a degree Celsius.
///
/// The value is an integer carried in an `f64`, exact at any magnitude a
/// temperature reaches, so two of them order exactly. A NaN input stays
/// NaN, and every comparison against it is false.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct TemperatureDelta(f64);

impl TemperatureDelta {
    /// A threshold or tolerance in °C, rounded to 0.001 °C.
    #[must_use]
    pub fn of(celsius: f64) -> Self {
        Self((celsius * MILLIDEGREES_PER_DEGREE).round())
    }

    /// How much warmer `a` is than `b` (°C, negative when colder),
    /// rounded to 0.001 °C.
    #[must_use]
    pub fn between(a: f64, b: f64) -> Self {
        Self::of(a - b)
    }

    /// The size of the difference, whichever way it points.
    #[must_use]
    pub const fn abs(self) -> Self {
        Self(self.0.abs())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_difference_of_exactly_a_threshold_equals_it() {
        // Bare, these differences read 0.3000000000000007,
        // 0.4999999999999991, 2.299999999999997 and 0.4999999999999982.
        for (a, b, threshold) in [
            (-19.7, -20.0, 0.3),
            (-7.7, -8.2, 0.5),
            (-30.0, -32.3, 2.3),
            (16.4, 15.9, 0.5),
        ] {
            assert_eq!(
                TemperatureDelta::between(a, b),
                TemperatureDelta::of(threshold),
                "{a} - {b} against {threshold}"
            );
        }
    }

    #[test]
    fn a_difference_of_f32_readings_absorbs_their_residue() {
        // A ZWO EAF reports an f32: 15.8 and 16.3 widen to
        // 15.800000190734863 and 16.299999237060547, 0.4999990463256836
        // apart.
        let (from, to) = (f64::from(15.8_f32), f64::from(16.3_f32));
        assert_eq!(
            TemperatureDelta::between(to, from),
            TemperatureDelta::of(0.5)
        );
    }

    #[test]
    fn a_reading_one_tenth_past_a_threshold_stays_past_it() {
        // Every tolerance from 0.1 to 3.0 °C against every rung on the
        // cooling grid, with readings in a ZWO camera's tenths: exactly
        // rung + tolerance equals the tolerance, and one tenth either
        // side is strictly inside or outside it.
        for tolerance_tenths in 1..=30 {
            let tolerance = TemperatureDelta::of(f64::from(tolerance_tenths) / 10.0);
            for rung in (-40..=15).step_by(5) {
                let rung_c = f64::from(rung);
                let reading = |offset: i32| f64::from(rung * 10 + tolerance_tenths + offset) / 10.0;
                assert_eq!(
                    TemperatureDelta::between(reading(0), rung_c),
                    tolerance,
                    "rung {rung}, tolerance {tolerance_tenths} tenths"
                );
                assert!(TemperatureDelta::between(reading(-1), rung_c) < tolerance);
                assert!(TemperatureDelta::between(reading(1), rung_c) > tolerance);
            }
        }
    }

    #[test]
    fn abs_is_the_size_of_a_difference_either_way() {
        assert_eq!(
            TemperatureDelta::between(15.9, 16.4).abs(),
            TemperatureDelta::of(0.5)
        );
        assert_eq!(
            TemperatureDelta::between(16.4, 15.9).abs(),
            TemperatureDelta::of(0.5)
        );
    }
}

//! The `f64` an ASCOM member reports for a C `float` a vendor SDK hands over.
//!
//! A vendor SDK that reports a decimal quantity as a C `float` — the ZWO EAF's
//! temperature, an ASI camera's electrons-per-ADU, the pixel size
//! `SVBGetSensorPixelSize` writes — means a decimal: `24.54` °C, `4.96` e⁻/ADU,
//! `3.76` µm. The `float`
//! holds only the binary value nearest to it, and `f64::from` widens that
//! binary value exactly, so `4.96f32` would reach a client as
//! `4.960000038146973`. A client that prints it shows the residue, and one
//! that subtracts two readings and compares the difference against a threshold
//! can land on the wrong side of it: `16.55f32` and `15.55f32` widened differ
//! by `0.9999990463256836`, short of a 1 °C threshold their decimals meet.
//!
//! [`to_f64`] reports the decimal instead: the shortest decimal that reads back
//! as the same `float`, parsed as an `f64`. A decimal of at most six significant
//! digits in the `float`'s normal range survives the trip into a `float` and
//! back unchanged, so whenever the SDK's value is such a decimal, [`to_f64`]
//! returns exactly the `f64` that decimal parses to.

/// The `f64` for a C `float` a vendor SDK hands over: the shortest decimal that
/// reads back as `value`, rather than `value`'s exact binary expansion.
///
/// `4.96f32` gives `4.96`, where `f64::from` gives `4.960000038146973`. A NaN, an
/// infinity and a negative zero come back as themselves.
#[must_use]
pub fn to_f64(value: f32) -> f64 {
    // `Display` prints an `f32` as the shortest decimal that reads back as the
    // same `f32`, and every form it prints — NaN and the infinities included —
    // is one `f64::from_str` reads. The fallback, the `float`'s exact binary
    // value, is therefore never taken; it keeps the function total without a
    // panic.
    value
        .to_string()
        .parse::<f64>()
        .unwrap_or_else(|_| f64::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_float_reads_as_the_decimal_it_stands_for() {
        assert_eq!(to_f64(4.96), 4.96);
        assert_eq!(to_f64(24.54), 24.54);
        assert_eq!(to_f64(3.76), 3.76);
        assert_eq!(to_f64(0.00496), 0.00496);
    }

    #[test]
    fn every_hundredth_of_a_degree_from_minus_40_to_60_reads_exactly() {
        for hundredths in -4_000_i32..=6_000 {
            let written = format!("{hundredths}e-2");
            // The SDK's float: the decimal rounded to the nearest f32, as a C
            // compiler rounds the literal `24.54f`.
            let sdk_float: f32 = written.parse().unwrap();
            let decimal: f64 = written.parse().unwrap();
            assert_eq!(to_f64(sdk_float), decimal, "{written}");
        }
    }

    #[test]
    fn readings_a_degree_apart_differ_by_exactly_one() {
        assert_eq!(to_f64(16.55) - to_f64(15.55), 1.0);
    }

    #[test]
    fn non_finite_values_and_negative_zero_come_back_as_themselves() {
        assert!(to_f64(f32::NAN).is_nan());
        assert_eq!(to_f64(f32::INFINITY), f64::INFINITY);
        assert_eq!(to_f64(f32::NEG_INFINITY), f64::NEG_INFINITY);
        let zero = to_f64(-0.0);
        assert_eq!(zero, 0.0);
        assert!(zero.is_sign_negative());
    }

    #[test]
    fn the_extremes_of_the_float_range_read_back_as_the_same_float() {
        for value in [f32::MAX, f32::MIN, f32::MIN_POSITIVE, f32::from_bits(1)] {
            #[expect(
                clippy::as_conversions,
                reason = "narrowing back to f32 is the round trip under test; std has no lossy From"
            )]
            let back = to_f64(value) as f32;
            assert_eq!(back.to_bits(), value.to_bits(), "{value:e}");
        }
    }
}

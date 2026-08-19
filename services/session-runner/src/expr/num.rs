//! The exact-integer boundary of the expression number model.

/// An `f64` holding an exact integer: zero fraction, magnitude at most
/// 2^53 — the threshold up to which every integer is exactly
/// representable in `f64` (2^53 + 1 is the first that is not). This is
/// the one boundary where expression arithmetic — which always produces
/// `f64` — meets integer-typed consumers: tool parameters, loop bounds,
/// array indices.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExactInt(f64);

impl ExactInt {
    /// 2^53 as `f64`: every integer up to this magnitude is exactly
    /// representable; 2^53 + 1 is the first that is not.
    pub(crate) const MAX_F64: f64 = 9_007_199_254_740_992.0;

    /// `None` unless `f` is an exact integer within ±2^53 (NaN and the
    /// infinities fail the fraction test).
    pub(crate) fn try_from_f64(f: f64) -> Option<Self> {
        (f.fract() == 0.0 && f.abs() <= Self::MAX_F64).then_some(Self(f))
    }

    /// The integer value. Exact: the constructor bounds the magnitude
    /// well inside `i64`'s range and excludes fractions, so the cast
    /// neither truncates nor saturates.
    #[expect(
        clippy::as_conversions,
        reason = "the constructor invariant makes this cast exact; no fallible f64-to-i64 conversion exists in std"
    )]
    pub(crate) const fn as_i64(self) -> i64 {
        self.0 as i64
    }

    /// The value as a `u64`; `None` when negative.
    pub(crate) fn to_u64(self) -> Option<u64> {
        u64::try_from(self.as_i64()).ok()
    }

    /// The value as a `usize` index; `None` when negative or, on 32-bit
    /// targets, beyond `usize`.
    pub(crate) fn to_usize(self) -> Option<usize> {
        usize::try_from(self.as_i64()).ok()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn accepts_exact_integers_up_to_the_representable_edge() {
        assert_eq!(ExactInt::try_from_f64(0.0).unwrap().as_i64(), 0);
        assert_eq!(ExactInt::try_from_f64(-3.0).unwrap().as_i64(), -3);
        assert_eq!(
            ExactInt::try_from_f64(ExactInt::MAX_F64).unwrap().as_i64(),
            9_007_199_254_740_992
        );
        assert_eq!(
            ExactInt::try_from_f64(-ExactInt::MAX_F64).unwrap().as_i64(),
            -9_007_199_254_740_992
        );
    }

    #[test]
    fn rejects_fractions_nan_infinities_and_the_out_of_range() {
        assert_eq!(ExactInt::try_from_f64(1.5), None);
        assert_eq!(ExactInt::try_from_f64(f64::NAN), None);
        assert_eq!(ExactInt::try_from_f64(f64::INFINITY), None);
        assert_eq!(ExactInt::try_from_f64(f64::NEG_INFINITY), None);
        assert_eq!(ExactInt::try_from_f64(1.0e16), None);
    }

    #[test]
    fn unsigned_views_reject_negatives() {
        assert_eq!(ExactInt::try_from_f64(7.0).unwrap().to_u64(), Some(7));
        assert_eq!(ExactInt::try_from_f64(-1.0).unwrap().to_u64(), None);
        assert_eq!(ExactInt::try_from_f64(7.0).unwrap().to_usize(), Some(7));
        assert_eq!(ExactInt::try_from_f64(-1.0).unwrap().to_usize(), None);
    }
}

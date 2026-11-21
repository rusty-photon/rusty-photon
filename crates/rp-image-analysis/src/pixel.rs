//! `Pixel` trait — the abstraction analysis algorithms are generic over.
//!
//! Cameras emit either `u16` (every consumer/prosumer astro camera) or `i32`
//! (a scientific camera whose values exceed the `u16` range, such as an
//! sCMOS HDR mode; `rp`'s image cache holds both).
//! Each analysis algorithm is written once over `T: Pixel` and monomorphized
//! for both types.

/// Pixel value of a frame, as `rp`'s image cache holds it. Implementations exist
/// for `u16` (the primary path) and `i32` (the scientific-camera hatch).
///
/// `to_f64` covers the bulk of analysis arithmetic (means, sigma-clipping,
/// centroids, HFR). `to_u32` is for camera-`max_adu` saturation comparison.
pub trait Pixel: Copy + Send + Sync + 'static {
    /// Convert to `f64` for analysis arithmetic.
    fn to_f64(self) -> f64;

    /// Convert to `u32` for saturation comparison against the camera's
    /// `max_adu`. Negative `i32` values clamp to `0` — they are not
    /// physically meaningful as photoelectron counts.
    fn to_u32(self) -> u32;
}

impl Pixel for u16 {
    #[inline]
    fn to_f64(self) -> f64 {
        f64::from(self)
    }

    #[inline]
    fn to_u32(self) -> u32 {
        u32::from(self)
    }
}

impl Pixel for i32 {
    #[inline]
    fn to_f64(self) -> f64 {
        f64::from(self)
    }

    #[inline]
    fn to_u32(self) -> u32 {
        self.max(0).cast_unsigned()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn u16_pixel_conversions() {
        assert_eq!(<u16 as Pixel>::to_f64(0), 0.0);
        assert_eq!(<u16 as Pixel>::to_f64(65535), 65535.0);
        assert_eq!(<u16 as Pixel>::to_u32(0), 0);
        assert_eq!(<u16 as Pixel>::to_u32(65535), 65535);
    }

    #[test]
    fn i32_pixel_conversions() {
        assert_eq!(<i32 as Pixel>::to_f64(0), 0.0);
        assert_eq!(<i32 as Pixel>::to_f64(100_000), 100_000.0);
        assert_eq!(<i32 as Pixel>::to_u32(0), 0);
        assert_eq!(<i32 as Pixel>::to_u32(100_000), 100_000);
    }

    #[test]
    fn i32_negative_clamps_to_zero_for_u32() {
        assert_eq!(<i32 as Pixel>::to_u32(-1), 0);
        assert_eq!(<i32 as Pixel>::to_u32(i32::MIN), 0);
    }
}

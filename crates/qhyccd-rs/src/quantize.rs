//! Float → integer conversions, gathered so the policy is stated once instead of
//! assumed at each site: `as` truncates toward zero and saturates at the
//! destination's bounds, with NaN landing on zero. No `TryFrom<f64>` exists to
//! spell that another way, which is why this module is the crate's only
//! `as_conversions` exemption rather than one per call.
//!
//! Both backends need it. The SDK carries every control value as an `f64`
//! (`GetQHYCCDParam` / `SetQHYCCDParam`), so the real accessors quantize the
//! ones that are integers underneath — a slot, a bit depth, an exposure in
//! microseconds — and the simulated image generators quantize the pixel maths
//! they compute in floating point.

#![expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "no TryFrom<f64> exists; `as` truncates and saturates (NaN to zero), and naming that policy here is the point of the module"
)]

/// The only width both backends need: a slot count and a slot position, which
/// the real accessors read back out of the SDK's `f64` control values.
pub const fn to_u32(v: f64) -> u32 {
    v as u32
}

// The remaining widths belong to the simulated backend — pixel samples, buffer
// lengths, an exposure in microseconds — so they are gated with it rather than
// left to warn as dead code in a real-SDK build.
#[cfg(feature = "simulation")]
pub const fn to_u8(v: f64) -> u8 {
    v as u8
}
#[cfg(feature = "simulation")]
pub const fn to_u16(v: f64) -> u16 {
    v as u16
}
#[cfg(feature = "simulation")]
pub const fn to_u64(v: f64) -> u64 {
    v as u64
}
#[cfg(feature = "simulation")]
pub const fn to_usize(v: f64) -> usize {
    v as usize
}
#[cfg(feature = "simulation")]
pub const fn to_i32(v: f64) -> i32 {
    v as i32
}

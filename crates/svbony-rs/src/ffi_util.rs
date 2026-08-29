//! Small shared helpers for reading values out of the raw `SVBony` SDK FFI.
//!
//! Only needed on the real-FFI path; the `simulation` backend fabricates its
//! values directly, so this module is compiled out under that feature.

/// Read a fixed-size, NUL-terminated C `char` buffer into an owned [`String`]
/// (lossy on invalid UTF-8). Portable across `c_char` signedness: the
/// byte-level reinterpretation is spelled via `from_ne_bytes` because a plain
/// `as u8` cast trips `clippy::unnecessary_cast` on the targets where `c_char`
/// is already `u8` (e.g. aarch64-linux) while being required where it is `i8`.
pub fn c_string_field(buf: &[std::os::raw::c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| u8::from_ne_bytes(c.to_ne_bytes()))
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Widen a C `long` out of the raw SDK structs to `i64`.
///
/// On LP64 unix `c_long` already *is* `i64`, so clippy flags the conversion as
/// useless there — but on Windows (LLP64) and 32-bit targets it is `i32` and
/// the widening is required. The allow is therefore cfg'd to exactly the
/// targets where the lint fires and the conversion is a no-op.
#[cfg_attr(
    all(unix, target_pointer_width = "64"),
    allow(clippy::useless_conversion)
)]
pub fn c_long_field(v: std::os::raw::c_long) -> i64 {
    i64::from(v)
}

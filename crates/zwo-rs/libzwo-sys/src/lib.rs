//! Raw FFI bindings for the ZWO ASI camera, EFW filter wheel, and EAF focuser
//! SDK.
//!
//! The bindings are generated at build time by [`bindgen`] from the vendored MIT
//! headers in `sdk/include/` (`ASICamera2.h`, `EFW_filter.h`, `EAF_focuser.h`),
//! parsed as C++ so the EFW/EAF headers' bare `bool` resolves to the builtin
//! type. See `build.rs`.
//!
//! This is a `*-sys` crate: it exposes only the raw, unsafe bindings plus the
//! link directives. Use the safe [`zwo-rs`](https://crates.io/crates/zwo-rs)
//! wrapper instead.
//!
//! [`bindgen`]: https://crates.io/crates/bindgen
pub use bindings::*;

/// The bindings bindgen generates from the vendored headers, re-exported at
/// the crate root. Their lint suppressions sit on this module rather than on
/// the crate, so the hand-written code below is linted like any other.
mod bindings {
    #![allow(
        non_upper_case_globals,
        non_camel_case_types,
        non_snake_case,
        dead_code
    )]
    // Generated bindings are not idiomatic Rust, so the warn-level groups
    // stay off. Of the workspace's denied restriction lints (root Cargo.toml
    // [workspace.lints.clippy]), which no group contains, the bindings trip
    // two, and only in bindgen's layout checks. Each `const _: () = { .. }`
    // indexes one-element arrays by a type's `size_of` or `align_of`, or a
    // field's `offset_of!`, minus the value bindgen took from clang's layout
    // of the header. A wrong layout underflows the subtraction or indexes
    // past the array, and either fails the build.
    #![allow(clippy::all, clippy::pedantic, clippy::nursery)]
    #![expect(
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        reason = "bindgen's layout checks, evaluated at compile time: a wrong layout fails the build"
    )]

    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

/// Keeps `libudev` in the consumer binary's `DT_NEEDED`.
///
/// The INDI-vendored EFW/EAF blobs reference `udev_*` symbols **without**
/// declaring libudev in their own `DT_NEEDED` (verified with
/// `nm -D --undefined-only` on the x64 + armv8 blobs), so the *consumer
/// binary* must carry the libudev dependency for the loader to resolve them.
/// The `-ludev` directive from `build.rs` is not sufficient: the linker's
/// as-needed default drops a library no regular object references. This
/// `#[used]` function-pointer static is that regular-object reference. The
/// `zwo_keep_udev` cfg is emitted by `build.rs` exactly when the udev link
/// directive is (Linux, `efw`/`focuser` features, not `ZWO_SKIP_NATIVE_LINK`).
#[cfg(zwo_keep_udev)]
mod udev_keepalive {
    extern "C" {
        fn udev_new() -> *mut std::os::raw::c_void;
    }
    #[used]
    #[no_mangle]
    static ZWO_SYS_KEEP_UDEV: unsafe extern "C" fn() -> *mut std::os::raw::c_void = udev_new;
}

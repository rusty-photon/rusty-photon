//! Windows delay-load link args for the proprietary `qhyccd.dll`.
//!
//! The QHYCCD Windows SDK's `qhyccd.lib` is an IMPORT library: without
//! intervention a missing `qhyccd.dll` (never shipped: the SDK's
//! redistribution rights are unresolved, and the operator's QHY All-in-One
//! pack provides it) kills the process in the Windows loader BEFORE `main` —
//! no log, just an error dialog. `/DELAYLOAD` defers binding to the first SDK call, so the
//! startup preflight (`src/preflight.rs`) can resolve the DLL and fail with an
//! actionable error instead. See docs/services/qhy-camera.md § "Windows:
//! qhyccd.dll resolution" (contracts WD1–WD3).
//!
//! The args live HERE, in the binary crate's build script — not in
//! `crates/qhyccd-rs/libqhyccd-sys/build.rs` next to the link directives —
//! because `cargo:rustc-link-arg` applies only to the emitting package's own
//! link targets and does NOT propagate from a dependency's build script to
//! the final binary (verified empirically; `rules_rust` likewise propagates
//! only `-l`/`-L` from dependency build scripts). The hand-written
//! BUILD.bazel mirrors these flags on the real-SDK targets via `rustc_flags`.

use std::env;

fn main() {
    // Only env re-runs are declared: this script reads no files. Feature
    // flags (CARGO_FEATURE_*) are part of the build-script fingerprint, so
    // they need no rerun-if declaration.
    for var in [
        "QHYCCD_SKIP_NATIVE_LINK",
        "CARGO_CFG_TARGET_OS",
        "CARGO_CFG_TARGET_ENV",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    // `simulation` cfg's out the real FFI, so no qhyccd.dll imports exist to
    // delay-load (and /DELAYLOAD with zero imports draws LNK4199).
    let simulation = env::var_os("CARGO_FEATURE_SIMULATION").is_some();
    // The simulation-only escape hatch that omits the SDK link entirely — see
    // crates/qhyccd-rs/libqhyccd-sys/build.rs.
    let skip_native_link = env::var_os("QHYCCD_SKIP_NATIVE_LINK").is_some();

    // /DELAYLOAD is MSVC link.exe syntax; delayimp.lib is the MSVC delay-load
    // helper. The windows-gnu toolchain is unsupported for the real SDK link.
    if target_os == "windows" && target_env == "msvc" && !simulation && !skip_native_link {
        println!("cargo:rustc-link-arg=/DELAYLOAD:qhyccd.dll");
        println!("cargo:rustc-link-arg=delayimp.lib");
    }
}

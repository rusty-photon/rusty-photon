//! Build script for `libzwo-sys`.
//!
//! 1. Generates raw FFI bindings with `bindgen` from the vendored MIT headers in
//!    `sdk/include/` (parsed as C++ for the EFW/EAF `bool`). The bindings always
//!    cover the whole header set — bare extern declarations force no linkage, so
//!    generation is not feature-gated.
//! 2. Emits the link directives for the system-installed ZWO SDK, **gated per
//!    device feature** (ADR-014): `camera` → `libASICamera2` (+ `libusb-1.0`),
//!    `efw` → `libEFWFilter`, `focuser` → `libEAFFocuser`, plus the C++ runtime
//!    whenever any device SDK is linked.
//!
//! Mirrors `libqhyccd-sys`'s system-installed-SDK model: with a device feature
//! on, the link is emitted, so building/linking (`cargo build`/`test`) requires
//! that SDK on the link path — even with the `simulation` feature. `cargo
//! check`/`clippy` (no link step) only need libclang for bindgen.
//!
//! Two env overrides:
//! - `ZWO_SDK_LIB_DIR=/path/to/lib` — add an SDK search path.
//! - `ZWO_SKIP_NATIVE_LINK=1` — omit the native link directives entirely, for
//!   builds that exercise only the pure-Rust `simulation` path and provision no
//!   SDK (sanitizer CI). See [`emit_link_directives`].

use std::{env, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Register the custom cfg so `unexpected_cfgs` stays effective for real
    // typos. Deliberately the legacy single-colon `cargo:` syntax: this crate's
    // published MSRV is 1.70 (below check-cfg's Cargo 1.80), and older Cargos
    // warn-and-ignore unknown single-colon directives (the `cargo::` form would
    // hard-error there). On rustc < 1.80 the lint doesn't exist, so nothing is
    // lost on the MSRV path.
    println!("cargo:rustc-check-cfg=cfg(zwo_keep_udev)");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let include_dir = manifest_dir.join("sdk").join("include");
    let wrapper = manifest_dir.join("wrapper.h");

    // --- 1. bindgen -------------------------------------------------------
    println!("cargo:rerun-if-changed={}", wrapper.display());
    println!("cargo:rerun-if-changed={}", include_dir.display());

    let bindings = bindgen::Builder::default()
        .header(wrapper.to_string_lossy())
        // The EFW/EAF headers use bare `bool` without <stdbool.h>; parse as C++
        // so it resolves to the builtin type (ASICamera2.h parses fine either
        // way). Verified to produce clean, compiling bindings.
        .clang_args(["-x", "c++", "-std=c++14"])
        .clang_arg(format!("-I{}", include_dir.display()))
        // Only the ZWO SDK surface — keep stdlib/system symbols out.
        .allowlist_function("(ASI|EFW|EAF).*")
        .allowlist_type("_?(ASI|EFW|EAF).*")
        .allowlist_var("(ASI|EFW|EAF).*")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .map_err(|e| format!("bindgen failed to generate ZWO SDK bindings: {e}"))?;

    let out_path = PathBuf::from(env::var("OUT_DIR")?).join("bindings.rs");
    bindings
        .write_to_file(&out_path)
        .map_err(|e| format!("failed to write bindings.rs: {e}"))?;

    // --- 2. link directives ----------------------------------------------
    emit_link_directives();
    Ok(())
}

fn emit_link_directives() {
    // Allow the native link to be suppressed entirely. A `simulation` build
    // references no ASI/EFW/EAF symbols — the real FFI calls are
    // `#[cfg(not(feature = "simulation"))]`, and bare `extern` declarations do
    // not force the linker to resolve a library — so with these directives
    // omitted the crate links with no native SDK present. Used by the sanitizer
    // CI job, which exercises only the pure-Rust simulation path and provisions
    // no SDK. Env-gated, *not* feature-gated: a Cargo feature would be turned on
    // by `--all-features` everywhere and silently stop every such build from
    // linking the real SDK. (The per-device `camera`/`efw`/`focuser` features
    // below are safe as features because they are additive: `--all-features`
    // simply links every SDK, the pre-split behaviour.)
    println!("cargo:rerun-if-env-changed=ZWO_SKIP_NATIVE_LINK");
    if env::var_os("ZWO_SKIP_NATIVE_LINK").is_some() {
        println!(
            "cargo:warning=ZWO_SKIP_NATIVE_LINK set — omitting ZWO SDK link directives; \
             this is a simulation-only build that links no native SDK"
        );
        return;
    }

    // Per-device link gates (ADR-014). Each ZWO device SDK is an independent
    // library with no shared handle, so a consumer links only what it talks to:
    // zwo-camera builds with `camera`, zwo-focuser with `focuser`. With no
    // device feature on there is nothing to link at all.
    let camera = env::var_os("CARGO_FEATURE_CAMERA").is_some();
    let efw = env::var_os("CARGO_FEATURE_EFW").is_some();
    let focuser = env::var_os("CARGO_FEATURE_FOCUSER").is_some();
    if !(camera || efw || focuser) {
        return;
    }

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // Allow an explicit override of the SDK lib directory.
    println!("cargo:rerun-if-env-changed=ZWO_SDK_LIB_DIR");
    if let Ok(dir) = env::var("ZWO_SDK_LIB_DIR") {
        println!("cargo:rustc-link-search=native={dir}");
    }

    // Support-library map, from the blobs' undefined-symbol tables (nm -D
    // --undefined-only; verified on the x86_64 + aarch64 INDI blobs,
    // 2026-07-10). DT_NEEDED alone is NOT the ground truth here: the EFW/EAF
    // blobs reference udev_* symbols WITHOUT declaring libudev in their
    // DT_NEEDED, so the executable must link -ludev itself or the final link
    // fails with --no-allow-shlib-undefined. libASICamera2 references
    // libusb_* (and does declare it); the HID-family blobs (EFW/EAF)
    // reference udev only.
    match target_os.as_str() {
        "macos" => {
            println!("cargo:rustc-link-search=native=/usr/local/lib");
            // Homebrew libusb (Apple Silicon vs Intel).
            let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
            if arch == "aarch64" {
                println!("cargo:rustc-link-search=native=/opt/homebrew/lib");
            }
            if camera {
                println!("cargo:rustc-link-lib=dylib=ASICamera2");
                println!("cargo:rustc-link-lib=dylib=usb-1.0");
            }
            if efw {
                println!("cargo:rustc-link-lib=dylib=EFWFilter");
            }
            if focuser {
                println!("cargo:rustc-link-lib=dylib=EAFFocuser");
            }
            // The SDK dylibs are C++; the EFW/EAF USB-HID path uses
            // IOKit/CoreFoundation on macOS (frameworks are always present, so
            // emitting them for any device is harmless).
            println!("cargo:rustc-link-lib=dylib=c++");
            println!("cargo:rustc-link-lib=framework=IOKit");
            println!("cargo:rustc-link-lib=framework=CoreFoundation");
        }
        "windows" => {
            // ZWO ships per-arch import libs; assume the SDK lib dir is on the
            // search path (or set via ZWO_SDK_LIB_DIR).
            if camera {
                println!("cargo:rustc-link-lib=dylib=ASICamera2");
            }
            if efw {
                println!("cargo:rustc-link-lib=dylib=EFWFilter");
            }
            if focuser {
                println!("cargo:rustc-link-lib=dylib=EAFFocuser");
            }
        }
        _ => {
            // Linux and other Unix-like systems.
            println!("cargo:rustc-link-search=native=/usr/local/lib");
            if camera {
                println!("cargo:rustc-link-lib=dylib=ASICamera2");
                println!("cargo:rustc-link-lib=dylib=usb-1.0");
            }
            if efw {
                println!("cargo:rustc-link-lib=dylib=EFWFilter");
            }
            if focuser {
                println!("cargo:rustc-link-lib=dylib=EAFFocuser");
            }
            if efw || focuser {
                println!("cargo:rustc-link-lib=dylib=udev");
                // `-ludev` alone is not enough: the linker's as-needed default
                // records a DT_NEEDED only for libraries that satisfy a
                // reference from a REGULAR object, and only the blobs (other
                // DSOs) reference udev. Without a DT_NEEDED on the consumer
                // binary the loader never loads libudev, and resolving the
                // blob's udev_* symbols fails at process start (BIND_NOW) —
                // exactly what scripts/verify-packages.sh caught. This cfg
                // turns on the `#[used]` keep-alive reference in src/lib.rs,
                // giving the linker that regular-object reference.
                println!("cargo:rustc-cfg=zwo_keep_udev");
            }
            // All three SDK blobs are C++.
            println!("cargo:rustc-link-lib=dylib=stdc++");
        }
    }
}

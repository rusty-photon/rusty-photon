//! Build script for `libsvbony-sys`.
//!
//! `lib.rs` is a hand-written `extern "C"` block (no bindgen — `SVBony`'s SDK
//! header carries no license text anywhere, see the crate docs), so this
//! script's main job is emitting the native link directives for the
//! system-installed `libSVBCameraSDK` (+ `libusb-1.0`). On macOS/Linux it
//! also emits the `svbony_keep_libusb` cfg, which turns on a `#[used]`
//! keep-alive reference in `lib.rs` — see that cfg's doc comment there for
//! why `-lusb-1.0` alone is not enough (issue #681).
//!
//! Two env overrides, mirroring `libqhyccd-sys`/`libzwo-sys`:
//! - `SVBONY_SDK_LIB_DIR=/path/to/lib` — add an explicit SDK search path.
//! - `SVBONY_SKIP_NATIVE_LINK=1` — omit the native link directives entirely
//!   (and the `#[link(...)]` attribute in `lib.rs`, via the
//!   `svbony_skip_link` cfg it sets), for builds that exercise only the
//!   pure-Rust `simulation` path and provision no SDK.
//!
//! Windows: sourced from `SVBony`'s own SDK download
//! (svbony.com/downloads/software-driver), not indi-3rdparty, whose packaging
//! covers Linux/macOS only. Its `.lib`/`.dll` (x86 + x64) export plain,
//! undecorated `cdecl` names matching this crate's `extern "C"` bindings, and
//! reference no `libusb` — the DLL drives the camera through Windows' in-box
//! `WinUSB`. Like the Linux/macOS blob it ships no license text (ADR-018). The
//! vendor download is captcha-gated (not scriptable), so CI provisions it
//! from the project's private token-gated mirror instead
//! (`install-svbony-sdk`'s Windows step, ADR-018 §7); builds without mirror
//! access set `SVBONY_SKIP_NATIVE_LINK`.

use std::env;

fn main() {
    for var in [
        "SVBONY_SKIP_NATIVE_LINK",
        "SVBONY_SDK_LIB_DIR",
        "CARGO_CFG_TARGET_OS",
        "CARGO_CFG_TARGET_ARCH",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }

    // Declare the cfgs the branches below may set, so `#[cfg_attr(not(svbony_skip_link),
    // ...)]` / `#[cfg(svbony_keep_libusb)]` in lib.rs do not trip the
    // `unexpected_cfgs` lint.
    println!("cargo:rustc-check-cfg=cfg(svbony_skip_link)");
    println!("cargo:rustc-check-cfg=cfg(svbony_keep_libusb)");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // Simulation-only escape hatch (mirrors QHYCCD_SKIP_NATIVE_LINK /
    // ZWO_SKIP_NATIVE_LINK): when set, emit NO link directives, so a
    // `--features simulation` build of svbony-rs — whose real FFI is
    // `#[cfg(not(feature = "simulation"))]` — links with no SVBony SDK
    // installed. Used by SDK-less dev builds and by every runner that cannot
    // provision the SDK. Checked before any OS branch, so a skip-link build —
    // which never touches the SDK, that being the point — succeeds on every
    // `CARGO_CFG_TARGET_OS`. `libsvbony-sys/BUILD.bazel` bakes it into every
    // Bazel target unconditionally, unlike libqhyccd-sys/libzwo-sys, whose
    // Bazel targets link the real pre-provisioned system SDK.
    if env::var_os("SVBONY_SKIP_NATIVE_LINK").is_some() {
        println!("cargo:rustc-cfg=svbony_skip_link");
        println!(
            "cargo:warning=SVBONY_SKIP_NATIVE_LINK set — omitting SVBony SDK link \
             directives; this is a simulation-only build that links no native SDK"
        );
        return;
    }

    // Allow an explicit override of the SDK lib directory (mirrors
    // ZWO_SDK_LIB_DIR/QHYCCD_SDK_LIB_DIR) — checked unconditionally, before
    // any OS-specific default, since Windows relies on it entirely (no
    // ldconfig/Homebrew-style default prefix exists there).
    if let Some(dir) = env::var("SVBONY_SDK_LIB_DIR")
        .ok()
        .filter(|d| !d.is_empty())
    {
        println!("cargo:rustc-link-search=native={dir}");
    }

    match target_os.as_str() {
        "macos" => {
            // Best-effort only: indi-3rdparty's libsvbony packaging carries
            // just a `mac64` (Intel) blob. No `mac_arm64` blob has been
            // confirmed via that source as of SDK 1.13.4 (SVBony's own direct
            // SDK download has not yet been byte-verified — see
            // docs/plans/archive/svbony-camera.md "Packaging"), so this link may fail
            // on Apple Silicon until that is checked/staged.
            println!("cargo:rustc-link-search=native=/usr/local/lib");
            let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
            if arch == "aarch64" {
                println!("cargo:rustc-link-search=native=/opt/homebrew/lib");
                println!(
                    "cargo:warning=No confirmed arm64 macOS libSVBCameraSDK blob as of SDK \
                     1.13.4 packaging (indi-3rdparty ships mac64/Intel only); this link may \
                     fail until an Apple Silicon SDK build is independently verified"
                );
            }
            println!("cargo:rustc-link-lib=dylib=SVBCameraSDK");
            println!("cargo:rustc-link-lib=dylib=usb-1.0");
            println!("cargo:rustc-cfg=svbony_keep_libusb");
        }
        "windows" => {
            // No default search path exists on Windows (no ldconfig, no
            // Homebrew prefix), so linking the real SDK relies entirely on
            // SVBONY_SDK_LIB_DIR (set above) or the consumer's own `-L` —
            // in CI, install-svbony-sdk's Windows step exports it (and puts
            // the DLL dir on PATH for runtime). No libusb link (unlike
            // Linux/macOS): the DLL uses Windows' in-box WinUSB.
            println!("cargo:rustc-link-lib=dylib=SVBCameraSDK");
        }
        _ => {
            // Linux (amd64/x86/armv6/armv7/armv8-aarch64, per indi-3rdparty's
            // libsvbony blobs). The vendored blob carries NO embedded
            // DT_SONAME (byte-verified; indi-3rdparty's `SOVERSION 1` is a
            // CMake install-time property only), but glibc's ldconfig falls
            // back to the on-disk filename as its cache key, so a
            // `libSVBCameraSDK.so` linker name in an ldconfig-scanned prefix
            // (how CI installs it) resolves `-lSVBCameraSDK` with no RUNPATH
            // trick; the packaged binary instead resolves the blob through
            // RUNPATH=/usr/lib/rusty-photon (ADR-018 §4).
            println!("cargo:rustc-link-search=native=/usr/local/lib");
            println!("cargo:rustc-link-lib=dylib=SVBCameraSDK");
            println!("cargo:rustc-link-lib=dylib=usb-1.0");
            println!("cargo:rustc-cfg=svbony_keep_libusb");
        }
    }
}

//! # zwo-rs — safe Rust bindings for the ZWO ASI camera, EFW filter wheel & EAF focuser SDK
//!
//! Sibling crate to [`qhyccd-rs`](https://crates.io/crates/qhyccd-rs). It wraps
//! the raw FFI in [`libzwo-sys`](https://crates.io/crates/libzwo-sys) — generated
//! by `bindgen` from the vendored MIT ZWO SDK headers — in a safe, ergonomic
//! API. It is consumed by rusty-photon's `zwo-camera` ASCOM Alpaca driver.
//!
//! ## Status
//!
//! **Under construction.** Enumeration, SDK-version queries, the ASI [`Camera`]
//! handle (open/init, [`CameraInfo`], serial, control caps, ROI and binning,
//! control get/set, single exposures, frame download, and ST4 guiding), the
//! EFW [`FilterWheel`] handle (open, slot count, position with the moving
//! sentinel, serial, firmware, calibration, direction), and the EAF [`Focuser`]
//! handle (open, `MaxStep`, position, dedicated `IsMoving`, absolute move,
//! stop, temperature, reverse, serial, firmware) are all wired to the FFI, per
//! the rusty-photon `docs/plans/zwo-driver.md` plan. Scope order: **Camera →
//! EFW filter wheel → EAF focuser**.
//!
//! ## Device features (`camera` / `efw` / `focuser`)
//!
//! The three ZWO device SDKs are independent libraries with no shared handle,
//! so each device surface is its own additive feature that compiles the matching
//! module ([`Camera`], [`FilterWheel`], [`Focuser`]) and forwards to the
//! matching `libzwo-sys` link feature. **Default = all three** (the pre-split
//! behaviour); narrow consumers (e.g. rusty-photon's `zwo-camera` /
//! `zwo-focuser` services) use `default-features = false` and pick one, so a
//! camera-only binary never links `libEFWFilter`/`libEAFFocuser` and vice
//! versa.
//!
//! ## `simulation` feature
//!
//! Mirrors qhyccd-rs: enables a hardware-free, in-Rust simulated environment for
//! development and tests. Note (as with qhyccd-rs) the SDK is still *linked* when
//! this feature is enabled — it removes the hardware, not the link. With the
//! feature on, the SDK is never called: enumeration reports the fixed simulated
//! device counts (`SIM_CAMERA_COUNT`, `SIM_FILTER_WHEEL_COUNT`,
//! `SIM_FOCUSER_COUNT` — each present only with its device feature).
//!
//! ## Build requirements
//!
//! - **libclang** — `libzwo-sys` runs `bindgen` at build time (needed for
//!   `check`/`clippy`/build; *not* the SDK).
//! - **The enabled ZWO SDK libraries** (`libASICamera2` for `camera` — plus
//!   **libusb-1.0** —, `libEFWFilter` for `efw`, `libEAFFocuser` for `focuser`)
//!   on the link path — needed to *link* (i.e. `build`/`test`), even with the
//!   `simulation` feature.
//! - **libudev** (Linux, `efw`/`focuser` only) — the EFW/EAF blobs reference
//!   `udev_*` symbols without declaring libudev in their own `DT_NEEDED`, so
//!   the consumer binary links it on their behalf (`libudev-dev` on
//!   Debian/Ubuntu, `systemd-devel` on Fedora). See the README.

// Curated test-scope allow list — documented in the root Cargo.toml
// [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        clippy::used_underscore_binding,
        clippy::significant_drop_tightening,
        clippy::significant_drop_in_scrutinee,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::suboptimal_flops,
        clippy::too_many_lines,
        clippy::option_if_let_else,
        clippy::match_same_arms,
        clippy::float_cmp,
        clippy::similar_names,
        clippy::struct_excessive_bools,
    )
)]

/// Raw, unsafe FFI bindings (`bindgen`). Prefer the safe API in this crate.
pub use libzwo_sys as sys;

#[cfg(feature = "camera")]
mod camera;
#[cfg(feature = "efw")]
mod efw;
mod error;
// Only needed by the real-FFI path of the per-device modules; compiled out
// under `simulation` and when no device feature is enabled.
#[cfg(all(
    not(feature = "simulation"),
    any(feature = "camera", feature = "efw", feature = "focuser")
))]
mod ffi_util;
#[cfg(feature = "focuser")]
mod focuser;
#[cfg(feature = "camera")]
pub use camera::{
    BayerPattern, Camera, CameraInfo, ControlCaps, ControlType, ControlValue, ExposureStatus,
    GuideDirection, ImageType, RoiFormat,
};
#[cfg(feature = "efw")]
pub use efw::{FilterWheel, FilterWheelInfo};
pub use error::{asi_check, eaf_check, efw_check, AsiError, EafError, EfwError, Error, Result};
#[cfg(feature = "focuser")]
pub use focuser::{Focuser, FocuserInfo};

/// Number of simulated ASI cameras presented when the `simulation` feature is on.
#[cfg(all(feature = "simulation", feature = "camera"))]
pub const SIM_CAMERA_COUNT: usize = 1;

/// Number of simulated EFW filter wheels presented when `simulation` is on.
#[cfg(all(feature = "simulation", feature = "efw"))]
pub const SIM_FILTER_WHEEL_COUNT: usize = 1;

/// Number of simulated EAF focusers presented when `simulation` is on.
#[cfg(all(feature = "simulation", feature = "focuser"))]
pub const SIM_FOCUSER_COUNT: usize = 1;

/// Entry point to the ZWO SDK.
///
/// Enumerates connected ASI cameras and EFW filter wheels. With the `simulation`
/// feature, a fixed simulated environment is reported and the native SDK is
/// never called (though it is still linked — see the crate docs).
#[derive(Debug, Default)]
pub struct Sdk {
    _private: (),
}

impl Sdk {
    /// Initialise the SDK.
    ///
    /// # Errors
    /// Currently infallible, but returns [`Result`] so future initialisation
    /// (e.g. SDK version checks) can surface failures without an API break.
    pub fn new() -> Result<Self> {
        tracing::debug!("initialising ZWO SDK");
        Ok(Self { _private: () })
    }

    /// Number of connected ASI cameras (`ASIGetNumOfConnectedCameras`).
    ///
    /// # Errors
    /// Infallible today; returns [`Result`] for forward compatibility.
    #[cfg(feature = "camera")]
    // Const only under the simulation cfg; the real body calls into the SDK.
    #[allow(clippy::missing_const_for_fn)]
    pub fn camera_count(&self) -> Result<usize> {
        #[cfg(feature = "simulation")]
        let count = SIM_CAMERA_COUNT;
        #[cfg(not(feature = "simulation"))]
        let count = {
            // SAFETY: `ASIGetNumOfConnectedCameras` takes no arguments and
            // returns the connected-camera count (it probes USB and is always
            // safe to call). A negative return is clamped to zero.
            let n = unsafe { sys::ASIGetNumOfConnectedCameras() };
            usize::try_from(n).unwrap_or(0)
        };
        Ok(count)
    }

    /// Number of connected EFW filter wheels (`EFWGetNum`).
    ///
    /// # Errors
    /// Infallible today; returns [`Result`] for forward compatibility.
    #[cfg(feature = "efw")]
    // Const only under the simulation cfg; the real body calls into the SDK.
    #[allow(clippy::missing_const_for_fn)]
    pub fn filter_wheel_count(&self) -> Result<usize> {
        #[cfg(feature = "simulation")]
        let count = SIM_FILTER_WHEEL_COUNT;
        #[cfg(not(feature = "simulation"))]
        let count = {
            // SAFETY: `EFWGetNum` takes no arguments and returns the connected
            // filter-wheel count; always safe to call. Negative is clamped.
            let n = unsafe { sys::EFWGetNum() };
            usize::try_from(n).unwrap_or(0)
        };
        Ok(count)
    }

    /// ASI camera SDK version string (`ASIGetSDKVersion`), e.g. `"1, 36, 0"`.
    ///
    /// # Errors
    /// Infallible today; returns [`Result`] for forward compatibility.
    #[cfg(feature = "camera")]
    pub fn asi_version(&self) -> Result<String> {
        #[cfg(feature = "simulation")]
        let version = "simulation".to_owned();
        #[cfg(not(feature = "simulation"))]
        let version = {
            // SAFETY: `ASIGetSDKVersion` returns a pointer to a static,
            // NUL-terminated C string owned by the SDK; we only read it.
            let ptr = unsafe { sys::ASIGetSDKVersion() };
            version_string(ptr)
        };
        Ok(version)
    }

    /// EFW filter-wheel SDK version string (`EFWGetSDKVersion`).
    ///
    /// # Errors
    /// Infallible today; returns [`Result`] for forward compatibility.
    #[cfg(feature = "efw")]
    pub fn efw_version(&self) -> Result<String> {
        #[cfg(feature = "simulation")]
        let version = "simulation".to_owned();
        #[cfg(not(feature = "simulation"))]
        let version = {
            // SAFETY: as `asi_version` — a static, SDK-owned NUL-terminated
            // string we only read.
            let ptr = unsafe { sys::EFWGetSDKVersion() };
            version_string(ptr)
        };
        Ok(version)
    }

    /// Number of connected EAF focusers (`EAFGetNum`).
    ///
    /// # Errors
    /// Infallible today; returns [`Result`] for forward compatibility.
    #[cfg(feature = "focuser")]
    // Const only under the simulation cfg; the real body calls into the SDK.
    #[allow(clippy::missing_const_for_fn)]
    pub fn focuser_count(&self) -> Result<usize> {
        #[cfg(feature = "simulation")]
        let count = SIM_FOCUSER_COUNT;
        #[cfg(not(feature = "simulation"))]
        let count = {
            // SAFETY: `EAFGetNum` takes no arguments and returns the connected
            // focuser count; always safe to call. Negative is clamped.
            let n = unsafe { sys::EAFGetNum() };
            usize::try_from(n).unwrap_or(0)
        };
        Ok(count)
    }

    /// EAF focuser SDK version string (`EAFGetSDKVersion`).
    ///
    /// # Errors
    /// Infallible today; returns [`Result`] for forward compatibility.
    #[cfg(feature = "focuser")]
    pub fn eaf_version(&self) -> Result<String> {
        #[cfg(feature = "simulation")]
        let version = "simulation".to_owned();
        #[cfg(not(feature = "simulation"))]
        let version = {
            // SAFETY: as `asi_version` — a static, SDK-owned NUL-terminated
            // string we only read.
            let ptr = unsafe { sys::EAFGetSDKVersion() };
            version_string(ptr)
        };
        Ok(version)
    }
}

/// Read an SDK-owned, NUL-terminated C string into an owned [`String`]
/// (lossy on invalid UTF-8). An empty string is returned for a null pointer.
#[cfg(all(
    not(feature = "simulation"),
    any(feature = "camera", feature = "efw", feature = "focuser")
))]
fn version_string(ptr: *const std::os::raw::c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: the SDK returns a pointer to a static, NUL-terminated string;
    // the read is bounded by the terminating NUL and the data outlives the call.
    unsafe { std::ffi::CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(feature = "simulation")]
pub mod simulation {
    //! Hardware-free, in-Rust simulation backend (no SDK calls).
    //!
    //! Enumeration of the simulated environment is reported by [`crate::Sdk`]
    //! via `SIM_CAMERA_COUNT` / `SIM_FILTER_WHEEL_COUNT` / `SIM_FOCUSER_COUNT`
    //! (each present only with its device feature). Simulated frames and EFW
    //! motion land with the Camera and filter-wheel device handles.
    use rand::RngExt;

    /// One 16-bit noise sample — a placeholder for simulated sensor frames.
    #[must_use]
    pub fn noise_sample() -> u16 {
        rand::rng().random()
    }

    /// Fill `buf` with simulated sensor noise as fast as possible.
    ///
    /// A full-frame ASI2600 frame is ~52 MiB and this runs in unoptimised test/CI
    /// builds. Two earlier approaches both tripped `ConformU`'s 10 s `StartExposure`
    /// timeout: a per-byte `rand::rng()` lookup (the original, >10 s), and a bulk
    /// [`rand::RngCore::fill_bytes`] (`ChaCha` is ~seconds for 52 MiB in debug). A
    /// rayon parallel fill is fast in isolation but grabs every core, so when
    /// several `ConformU` camera suites run in one job (conformu.yml) it starves the
    /// siblings *and* itself and re-trips the timeout on constrained (e.g. macOS)
    /// runners. Instead: a seeded xorshift64 — a few integer ops per 8 bytes, fast
    /// even in debug, single-core, no extra deps. Quality is irrelevant; this is
    /// placeholder sensor noise, seeded per frame so frames differ run-to-run.
    pub fn fill_noise(buf: &mut [u8]) {
        let mut state = rand::rng().random::<u64>() | 1;
        for chunk in buf.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            for (dst, src) in chunk.iter_mut().zip(state.to_le_bytes()) {
                *dst = src;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_new_succeeds() {
        Sdk::new().unwrap();
    }

    // Per-device enumeration/version tests: each gated on its device feature
    // (the Sdk surface itself is feature-gated per ADR-014). Without the
    // simulation feature these call the real SDK; with no hardware attached the
    // counts are zero, but the calls must not panic.

    #[cfg(feature = "camera")]
    #[test]
    fn camera_enumeration_returns_a_count() {
        let sdk = Sdk::new().unwrap();
        let cameras = sdk.camera_count().unwrap();
        #[cfg(feature = "simulation")]
        assert_eq!(cameras, SIM_CAMERA_COUNT);
        #[cfg(not(feature = "simulation"))]
        let _ = cameras;
    }

    #[cfg(feature = "efw")]
    #[test]
    fn filter_wheel_enumeration_returns_a_count() {
        let sdk = Sdk::new().unwrap();
        let wheels = sdk.filter_wheel_count().unwrap();
        #[cfg(feature = "simulation")]
        assert_eq!(wheels, SIM_FILTER_WHEEL_COUNT);
        #[cfg(not(feature = "simulation"))]
        let _ = wheels;
    }

    #[cfg(feature = "focuser")]
    #[test]
    fn focuser_enumeration_returns_a_count() {
        let sdk = Sdk::new().unwrap();
        let focusers = sdk.focuser_count().unwrap();
        #[cfg(feature = "simulation")]
        assert_eq!(focusers, SIM_FOCUSER_COUNT);
        #[cfg(not(feature = "simulation"))]
        let _ = focusers;
    }

    #[cfg(feature = "camera")]
    #[test]
    fn asi_sdk_version_is_non_empty() {
        assert_ne!(Sdk::new().unwrap().asi_version().unwrap(), "");
    }

    #[cfg(feature = "efw")]
    #[test]
    fn efw_sdk_version_is_non_empty() {
        assert_ne!(Sdk::new().unwrap().efw_version().unwrap(), "");
    }

    #[cfg(feature = "focuser")]
    #[test]
    fn eaf_sdk_version_is_non_empty() {
        assert_ne!(Sdk::new().unwrap().eaf_version().unwrap(), "");
    }

    #[test]
    fn asi_check_maps_known_and_unknown_codes() {
        asi_check(0).unwrap();
        assert_eq!(
            asi_check(1).unwrap_err(),
            Error::Asi(AsiError::InvalidIndex)
        );
        assert_eq!(
            asi_check(16).unwrap_err(),
            Error::Asi(AsiError::GeneralError)
        );
        assert_eq!(
            asi_check(999).unwrap_err(),
            Error::Asi(AsiError::Unknown(999))
        );
    }

    #[test]
    fn efw_check_maps_known_and_unknown_codes() {
        efw_check(0).unwrap();
        assert_eq!(efw_check(5).unwrap_err(), Error::Efw(EfwError::Moving));
        assert_eq!(efw_check(9).unwrap_err(), Error::Efw(EfwError::Closed));
        assert_eq!(
            efw_check(42).unwrap_err(),
            Error::Efw(EfwError::Unknown(42))
        );
    }

    #[test]
    fn eaf_check_maps_known_and_unknown_codes() {
        eaf_check(0).unwrap();
        assert_eq!(eaf_check(5).unwrap_err(), Error::Eaf(EafError::Moving));
        assert_eq!(eaf_check(9).unwrap_err(), Error::Eaf(EafError::Closed));
        assert_eq!(
            eaf_check(42).unwrap_err(),
            Error::Eaf(EafError::Unknown(42))
        );
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn simulation_noise_sample_runs() {
        // Any u16 is valid; just exercise the simulation path.
        let _ = simulation::noise_sample();
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn simulation_fill_noise_fills_whole_buffer() {
        // A small buffer is enough to exercise the parallel fill path; the
        // chunking is internal. Just assert it touches every byte (vanishingly
        // unlikely to stay all-zero) and respects the slice length.
        let mut buf = vec![0u8; 256 * 1024 + 7];
        simulation::fill_noise(&mut buf);
        assert!(buf.iter().any(|&b| b != 0));
    }
}

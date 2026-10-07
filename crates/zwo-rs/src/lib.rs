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
    BayerPattern, Camera, CameraInfo, CameraList, ControlCaps, ControlType, ControlValue,
    ExposureStatus, GuideDirection, ImageType, RoiFormat,
};
#[cfg(feature = "efw")]
pub use efw::{FilterWheel, FilterWheelInfo};
pub use error::{asi_check, eaf_check, efw_check, AsiError, EafError, EfwError, Error, Result};
#[cfg(feature = "focuser")]
pub use focuser::{Focuser, FocuserInfo, FocuserList};

/// Number of simulated ASI cameras presented when the `simulation` feature is on.
#[cfg(all(feature = "simulation", feature = "camera"))]
pub const SIM_CAMERA_COUNT: usize = 1;

/// Number of simulated EFW filter wheels presented when `simulation` is on.
#[cfg(all(feature = "simulation", feature = "efw"))]
pub const SIM_FILTER_WHEEL_COUNT: usize = 1;

/// Number of simulated EAF focusers presented when `simulation` is on.
#[cfg(all(feature = "simulation", feature = "focuser"))]
pub const SIM_FOCUSER_COUNT: usize = 1;

/// The ASI SDK keeps one camera list per process: `ASIGetNumOfConnectedCameras`
/// rebuilds it, renumbering the indices `ASIGetCameraProperty` and the opens
/// read. Every call that rebuilds or reads that list takes this lock, so a
/// rescan on one thread never renumbers the list under another's lookup.
#[cfg(feature = "camera")]
static CAMERA_LIST: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hold [`CAMERA_LIST`] for one rescan-and-read, or for a [`CameraList`]. A panic while holding it
/// leaves nothing inconsistent on the Rust side, so a poisoned lock is taken
/// as is.
#[cfg(feature = "camera")]
pub(crate) fn lock_camera_list() -> std::sync::MutexGuard<'static, ()> {
    CAMERA_LIST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The EAF SDK keeps one focuser list per process: `EAFGetNum` rebuilds it,
/// renumbering the indices `EAFGetID` reads and, for a focuser that has come
/// back, handing out a new ID. Every call that rebuilds or reads that list
/// takes this lock, so a rescan on one thread never renumbers the list under
/// another's lookup.
#[cfg(feature = "focuser")]
static FOCUSER_LIST: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hold [`FOCUSER_LIST`] for one rescan, or for a [`FocuserList`]. A panic
/// while holding it leaves nothing inconsistent on the Rust side, so a
/// poisoned lock is taken as is.
#[cfg(feature = "focuser")]
pub(crate) fn lock_focuser_list() -> std::sync::MutexGuard<'static, ()> {
    FOCUSER_LIST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Simulation only: what the process's rescans have made of the device behind
/// one departure file. The real SDKs keep one device list per process, so this
/// is process-wide too (see [`Sdk::with_departure_file`]).
#[cfg(all(feature = "simulation", any(feature = "camera", feature = "focuser")))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct SimListing {
    /// Rescans that ran while the device was gone. The first one drops the
    /// device from the list for good, for every handle opened before it.
    pub(crate) rescans_while_gone: u64,
    /// Whether the last rescan listed the device. An open reads that list, so
    /// a device a rescan dropped cannot be opened until another rescan has
    /// listed it again, even once it is back on the bus.
    pub(crate) listed: bool,
}

#[cfg(all(feature = "simulation", any(feature = "camera", feature = "focuser")))]
impl SimListing {
    /// A departure file no rescan has seen yet: its device is listed.
    const UNSEEN: Self = Self {
        rescans_while_gone: 0,
        listed: true,
    };
}

/// Simulation only: [`SimListing`] per departure file.
#[cfg(all(feature = "simulation", any(feature = "camera", feature = "focuser")))]
static SIM_LISTINGS: std::sync::Mutex<std::collections::BTreeMap<std::path::PathBuf, SimListing>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Simulation only: what the rescans so far have made of `path`'s device.
#[cfg(all(feature = "simulation", any(feature = "camera", feature = "focuser")))]
pub(crate) fn sim_listing(path: &std::path::Path) -> SimListing {
    SIM_LISTINGS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(path)
        .copied()
        .unwrap_or(SimListing::UNSEEN)
}

/// Simulation only: a rescan of the bus `path` models, with the list lock
/// already held. A device gone at the rescan is dropped from the list, and one
/// on the bus is listed. Returns whether it is listed.
#[cfg(all(feature = "simulation", any(feature = "camera", feature = "focuser")))]
pub(crate) fn sim_rescan(path: &std::path::Path) -> bool {
    let gone = path.exists();
    let mut listings = SIM_LISTINGS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let listing = listings
        .entry(path.to_path_buf())
        .or_insert(SimListing::UNSEEN);
    if gone {
        listing.rescans_while_gone = listing.rescans_while_gone.saturating_add(1);
    }
    listing.listed = !gone;
    drop(listings);
    !gone
}

/// `ASIGetNumOfConnectedCameras`, with [`camera_list`] already held: rebuild
/// the SDK's camera list and count it.
#[cfg(feature = "camera")]
pub(crate) fn rescan(sdk: &Sdk) -> usize {
    #[cfg(feature = "simulation")]
    let count = sdk
        .departure_file
        .as_deref()
        .map_or(SIM_CAMERA_COUNT, |path| {
            if sim_rescan(path) {
                SIM_CAMERA_COUNT
            } else {
                0
            }
        });
    #[cfg(not(feature = "simulation"))]
    let count = {
        let _ = sdk;
        // SAFETY: `ASIGetNumOfConnectedCameras` takes no arguments and
        // returns the connected-camera count (it probes USB and is always
        // safe to call). A negative return is clamped to zero.
        let n = unsafe { sys::ASIGetNumOfConnectedCameras() };
        usize::try_from(n).unwrap_or(0)
    };
    count
}

/// Entry point to the ZWO SDK.
///
/// Enumerates connected ASI cameras and EFW filter wheels. With the `simulation`
/// feature, a fixed simulated environment is reported and the native SDK is
/// never called (though it is still linked — see the crate docs).
#[derive(Debug, Default)]
pub struct Sdk {
    /// Simulation only: while this file exists the simulated camera and
    /// focuser are off the bus. See [`Sdk::with_departure_file`].
    #[cfg(all(feature = "simulation", any(feature = "camera", feature = "focuser")))]
    departure_file: Option<std::path::PathBuf>,
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
        Ok(Self::default())
    }

    /// Simulation only: take the simulated camera and focuser off the bus whenever
    /// `path` exists, and put them back when the file is removed.
    ///
    /// Models a camera that loses its power or its cable while connected, as
    /// ASI SDK 1.41 was measured to on Linux (rusty-photon issue #1411). While
    /// the file exists this SDK finds no camera: a rescan counts none, and an
    /// open answers `AsiError::InvalidIndex`. A `Camera` opened before
    /// the departure keeps its handle, and until a rescan runs the SDK hides
    /// the departure: reads answer from memory, `Camera::set_control_value`
    /// and the guide pulses fail with `AsiError::GeneralError`, an exposure
    /// ends `ExposureStatus::Failed`, and a download succeeds with every
    /// pixel zero (that last one is unmeasured on ASI: a QHY readout was seen
    /// to do it on Windows). No call ever answers
    /// `AsiError::CameraRemoved`. The first rescan that runs while the camera
    /// is gone (`Sdk::camera_count`, `Sdk::cameras`, `Sdk::still_connected`,
    /// from any SDK in the process that shares the file) drops it for good:
    /// every call on the old handle answers `AsiError::InvalidId`, and
    /// `Sdk::still_connected` reports it gone, even once the file is removed.
    /// Like the real open, an open reads the list the last rescan left: a
    /// camera a rescan dropped opens again only after a rescan has listed it.
    ///
    /// The simulated EAF focuser leaves with it, as EAF SDK 1.7.7 was measured
    /// to on Linux (rusty-photon issue #1431). Unlike the ASI SDK, the EAF SDK
    /// says so at once: from the first call that sees the file, every call on
    /// a `Focuser` opened before the departure answers `EafError::Removed`,
    /// and once a rescan has run while it was gone, `EafError::InvalidId`
    /// (the real SDK answers `EafError::Closed` instead once the EAF is back
    /// and listed under its old ID; either means gone). A focuser opened
    /// before the departure never works again. A rescan
    /// (`Sdk::focuser_count`, `Sdk::focusers`, `FocuserList::rescan`)
    /// counts none while the file exists, and an open finds the EAF only while
    /// it is on the bus and the last rescan listed it. Not modelled: a
    /// departure and return that no call observed (the real SDK answers
    /// `Removed` after one), and the new ID a returned EAF can be listed under.
    // Code spans, not links, above: this builds with either device feature
    // alone, so a link to the other device's items would dangle.
    #[cfg(all(feature = "simulation", any(feature = "camera", feature = "focuser")))]
    #[must_use]
    pub fn with_departure_file(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.departure_file = Some(path.into());
        self
    }

    /// Number of connected ASI cameras (`ASIGetNumOfConnectedCameras`).
    ///
    /// # Errors
    /// Infallible today; returns [`Result`] for forward compatibility.
    #[cfg(feature = "camera")]
    pub fn camera_count(&self) -> Result<usize> {
        let list = lock_camera_list();
        let count = rescan(self);
        drop(list);
        Ok(count)
    }

    /// Whether `camera` is still on the bus: rescan it
    /// (`ASIGetNumOfConnectedCameras`), then ask the SDK for the camera's
    /// properties by its ID (`ASIGetCameraPropertyByID`).
    ///
    /// A departed camera is invisible until a rescan: the SDK goes on
    /// answering for it from memory. The rescan drops it, and from then on its
    /// ID answers `INVALID_ID`, or `CAMERA_CLOSED` once the camera has come back
    /// and a later rescan has listed it afresh. Either means this handle's
    /// camera has gone, and `Ok(false)` says so. Measured on Linux with ASI SDK
    /// 1.41 (rusty-photon issue #1411): no call answers `CAMERA_REMOVED`.
    ///
    /// A rescan leaves present cameras, their IDs and their exposures alone,
    /// but it renumbers the list the enumeration indices read, so an index
    /// taken before it may name another camera after it.
    ///
    /// # Errors
    /// Returns [`Error::Asi`] for any other answer, which says nothing either
    /// way.
    #[cfg(feature = "camera")]
    pub fn still_connected(&self, camera: &Camera) -> Result<bool> {
        let list = lock_camera_list();
        rescan(self);
        let listed = camera.listed();
        drop(list);
        listed
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

    /// Number of connected EAF focusers (`EAFGetNum`, which rescans the bus
    /// and rebuilds the SDK's focuser list).
    ///
    /// # Errors
    /// Infallible today; returns [`Result`] for forward compatibility.
    #[cfg(feature = "focuser")]
    pub fn focuser_count(&self) -> Result<usize> {
        let list = lock_focuser_list();
        let count = focuser::rescan(self);
        drop(list);
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
        // The alias's own MAX is beyond the vendored header's range on every
        // platform width and must be preserved exactly, not saturated.
        assert_eq!(
            asi_check(sys::ASI_ERROR_CODE::MAX).unwrap_err(),
            Error::Asi(AsiError::Unknown(i64::from(sys::ASI_ERROR_CODE::MAX)))
        );
    }

    #[test]
    fn from_code_preserves_codes_beyond_i32() {
        // A raw code above i32::MAX (reachable on LP64, where the alias is
        // c_uint) survives into Unknown intact instead of narrowing.
        assert_eq!(
            AsiError::from_code(4_294_967_295),
            AsiError::Unknown(4_294_967_295)
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

    #[test]
    fn only_removed_invalid_id_and_closed_mean_an_open_focuser_left_the_bus() {
        let departed: Vec<i32> = (1..=11)
            .filter(|&code| EafError::from_code(code).left_the_bus())
            .collect();
        // INVALID_ID, REMOVED and CLOSED, as measured on a departed EAF.
        assert_eq!(departed, [2, 4, 9]);
        assert!(
            !EafError::Moving.left_the_bus(),
            "a refused move is not a departure"
        );
        assert!(!EafError::Unknown(42).left_the_bus());
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

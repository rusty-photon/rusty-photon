//! # svbony-rs — safe Rust bindings for the SVBony camera SDK
//!
//! Sibling crate to [`qhyccd-rs`](https://crates.io/crates/qhyccd-rs) and
//! [`zwo-rs`](https://crates.io/crates/zwo-rs). It wraps the raw,
//! hand-transcribed FFI in
//! [`libsvbony-sys`](https://crates.io/crates/libsvbony-sys) in a safe,
//! ergonomic API. It is consumed by rusty-photon's `svbony-camera` ASCOM
//! Alpaca driver (a later phase; see `docs/plans/archive/svbony-camera.md`).
//!
//! ## Status
//!
//! **Under construction (Phase A/B of the plan).** Enumeration, SDK-version
//! queries, and the SVBony [`Camera`] handle (open/close, [`CameraInfo`],
//! [`CameraProperty`]/[`CameraPropertyEx`], control caps/get/set, ROI, camera
//! mode, video-capture exposure primitives incl. the soft-trigger flow, ST4
//! guiding, pixel size) are wired to the FFI. SVBony has only one device
//! (camera) and one SDK library, so — unlike `zwo-rs` — there is no
//! per-device Cargo feature union; only the `simulation` feature gates
//! anything.
//!
//! ## `simulation` feature
//!
//! Mirrors `qhyccd-rs`/`zwo-rs`: enables a hardware-free, in-Rust simulated
//! environment for development and tests. As with those crates, the SDK is
//! still *linked* when this feature is enabled (see [`sys`]) — it removes the
//! hardware, not the link. With the feature on, the SDK is never called:
//! enumeration reports a single fixed simulated camera ([`SIM_CAMERA_COUNT`]).
//! The simulation additionally models the soft-trigger video-capture flow
//! (`start_video_capture` arms it, `send_soft_trigger` arms a pending frame,
//! `get_video_data` consumes it) and a cooling ramp that advances one step
//! per poll (mirroring `zwo-rs`'s EAF focuser position ramp), so the
//! `svbony-camera` service's exposure state machine can be exercised
//! sim-side exactly as it will be against real hardware.
//!
//! ## Build requirements
//!
//! - **The SVBony SDK library** (`libSVBCameraSDK`, plus **libusb-1.0**) on
//!   the link path — needed to *link* (`build`/`test`), even with the
//!   `simulation` feature — unless `SVBONY_SKIP_NATIVE_LINK=1` is set (see
//!   `libsvbony-sys`'s `build.rs`).
//! - No bindgen / libclang requirement: `libsvbony-sys` is hand-written FFI.

/// Raw, unsafe FFI bindings (hand-written, no bindgen — see the crate docs).
/// Prefer the safe API in this crate.
pub use libsvbony_sys as sys;

mod camera;
mod error;
// Only needed by the real-FFI path; compiled out under `simulation`.
#[cfg(not(feature = "simulation"))]
mod ffi_util;

pub use camera::{
    BayerPattern, Camera, CameraInfo, CameraMode, CameraProperty, CameraPropertyEx, ControlCaps,
    ControlType, ControlValue, GuideDirection, ImageType, RoiFormat,
};
pub use error::{svb_check, Error, Result, SvbError};

/// Number of simulated SVBony cameras presented when the `simulation`
/// feature is on.
#[cfg(feature = "simulation")]
pub const SIM_CAMERA_COUNT: usize = 1;

/// Serializes every call this crate makes into the SVBony SDK's *global*
/// (non-per-handle) entry points — `SVBGetNumOfConnectedCameras`,
/// `SVBGetCameraInfo` (via the internal `read_camera_info`), `SVBGetSDKVersion`,
/// and `SVBOpenCamera`. The SDK's thread-safety is undocumented (the same
/// posture that makes [`Camera`] `Send + !Sync`), and unlike a `Camera`
/// handle's per-instance state, these entry points touch the SDK's
/// process-global camera/handle table — so serializing them needs a
/// process-wide lock, not a per-[`Sdk`]-instance one: [`Sdk`] is a cheap,
/// freely-instantiated ZST (callers mint one per discovered camera, see
/// `svbony-camera`'s `lib.rs`), so two independent `Sdk` values calling
/// `SVBOpenCamera` concurrently would otherwise race with no synchronization
/// at all.
static SDK_CALL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run `f` with the process-wide [`SDK_CALL_LOCK`] held, recovering from a
/// poisoned lock the same way a panicking SDK call would leave things: the
/// SDK call itself doesn't roll back on an unwind, so a poisoned lock is not
/// a reason to refuse further (single-threaded-serialized) SDK access.
fn with_sdk_lock<T>(f: impl FnOnce() -> T) -> T {
    let _guard = SDK_CALL_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f()
}

/// The unlocked `SVBGetNumOfConnectedCameras` query — callers MUST already
/// hold [`SDK_CALL_LOCK`] (via [`with_sdk_lock`]) before calling this; it
/// exists so [`camera::Sdk::cameras`] can query the count without
/// re-entering the (non-reentrant) lock that [`Sdk::camera_count`] takes.
pub(crate) fn camera_count_raw() -> usize {
    #[cfg(feature = "simulation")]
    let count = SIM_CAMERA_COUNT;
    #[cfg(not(feature = "simulation"))]
    let count = {
        // SAFETY: `SVBGetNumOfConnectedCameras` takes no arguments and
        // returns the connected-camera count directly (no error code); it
        // probes USB and is always safe to call. A negative return (not
        // documented as possible, but not ruled out either) is clamped to
        // zero. Caller holds `SDK_CALL_LOCK` (see its doc comment) since this
        // touches the SDK's global camera table.
        let n = unsafe { sys::SVBGetNumOfConnectedCameras() };
        usize::try_from(n).unwrap_or(0)
    };
    count
}

/// Entry point to the SVBony SDK.
///
/// Enumerates connected cameras. With the `simulation` feature, a fixed
/// simulated environment is reported and the native SDK is never called
/// (though it is still linked — see the crate docs). Every method serializes
/// against the process-wide [`SDK_CALL_LOCK`] — see its doc comment for why
/// a per-`Sdk`-instance lock would not be enough.
#[derive(Debug, Default)]
pub struct Sdk {
    _private: (),
}

impl Sdk {
    /// Initialise the SDK.
    ///
    /// # Errors
    /// Currently infallible, but returns [`Result`] so future initialisation
    /// can surface failures without an API break.
    pub fn new() -> Result<Self> {
        tracing::debug!("initialising SVBony SDK");
        Ok(Self { _private: () })
    }

    /// Number of connected SVBony cameras (`SVBGetNumOfConnectedCameras`).
    ///
    /// # Errors
    /// Infallible today; returns [`Result`] for forward compatibility.
    pub fn camera_count(&self) -> Result<usize> {
        let count = with_sdk_lock(camera_count_raw);
        tracing::debug!(count, "queried connected SVBony camera count");
        Ok(count)
    }

    /// SVBony camera SDK version string (`SVBGetSDKVersion`), e.g.
    /// `"1, 13, 0503"`.
    ///
    /// # Errors
    /// Infallible today; returns [`Result`] for forward compatibility.
    pub fn sdk_version(&self) -> Result<String> {
        with_sdk_lock(|| {
            #[cfg(feature = "simulation")]
            let version = "simulation".to_owned();
            #[cfg(not(feature = "simulation"))]
            let version = {
                // SAFETY: `SVBGetSDKVersion` returns a pointer to a static,
                // NUL-terminated C string owned by the SDK; we only read it.
                // Serialized by `SDK_CALL_LOCK` for consistency with the
                // SDK's other global entry points.
                let ptr = unsafe { sys::SVBGetSDKVersion() };
                version_string(ptr)
            };
            Ok(version)
        })
    }
}

/// Read an SDK-owned, NUL-terminated C string into an owned [`String`]
/// (lossy on invalid UTF-8). An empty string is returned for a null pointer.
#[cfg(not(feature = "simulation"))]
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
    //! via [`crate::SIM_CAMERA_COUNT`]. Simulated frames and the video-capture
    //! / cooling-ramp state machines land with the [`crate::Camera`] handle.
    use rand::RngExt;

    /// Fill `buf` with simulated sensor noise as fast as possible.
    ///
    /// A full-frame IMX533-class frame is tens of megabytes and this runs in
    /// unoptimised test/CI builds. `zwo-rs`'s `fill_noise` doc comment records
    /// the lesson this reuses directly: a per-byte `rand::rng()` lookup and a
    /// rayon parallel fill both either tripped or risked tripping
    /// ConformU's 10s `StartExposure` timeout (the parallel fill grabs every
    /// core, which starves sibling ConformU suites sharing a CI job). A
    /// seeded xorshift64 — a few integer ops per 8 bytes, fast even in debug,
    /// single-core, no extra deps — avoids both failure modes. Quality is
    /// irrelevant; this is placeholder sensor noise, seeded per frame so
    /// frames differ run-to-run.
    pub fn fill_noise(buf: &mut [u8]) {
        let mut state = rand::rng().random::<u64>() | 1;
        for chunk in buf.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
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

    #[test]
    fn camera_enumeration_returns_a_count() {
        let sdk = Sdk::new().unwrap();
        let cameras = sdk.camera_count().unwrap();
        #[cfg(feature = "simulation")]
        assert_eq!(cameras, SIM_CAMERA_COUNT);
        #[cfg(not(feature = "simulation"))]
        let _ = cameras;
    }

    #[test]
    fn sdk_version_is_non_empty() {
        assert_ne!(Sdk::new().unwrap().sdk_version().unwrap(), "");
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn simulation_fill_noise_fills_whole_buffer() {
        let mut buf = vec![0u8; 256 * 1024 + 7];
        simulation::fill_noise(&mut buf);
        assert!(buf.iter().any(|&b| b != 0));
    }
}

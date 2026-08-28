//! The [`Camera`] device and its [`ControlType`] address space, plus the
//! real-hardware handle machinery.
//!
//! This is the device-file-major layout of the sibling `zwo-rs` / `svbony-rs`
//! crates: one file per device, with the camera's behaviour grouped into `impl`
//! blocks (lifecycle, configuration, info, imaging, parameters, readout modes).
//! It merges the former six-file `camera/` submodule split, `backend.rs` (the
//! `#[cfg(not(simulation))]` handle cell) and `control.rs` ([`ControlType`]).
//!
//! The real/simulated backend is chosen at **compile time** by the `simulation`
//! feature: every camera method forks into a `#[cfg(not(feature = "simulation"))]`
//! real arm (calling the `libqhyccd-sys` FFI via [`crate::sys`]) and a
//! `#[cfg(feature = "simulation")]` arm (driving a `SimulatedCameraState`).

use std::sync::Arc;

#[cfg(not(feature = "simulation"))]
use std::ffi::{c_char, CStr};

use parking_lot::RwLock;

use crate::quantize;
use crate::Result;
use crate::{CCDChipArea, CCDChipInfo, FrameInfo, QHYError, StreamMode};

#[cfg(not(feature = "simulation"))]
use crate::check;
#[cfg(not(feature = "simulation"))]
use crate::sys::{
    BeginQHYCCDLive, CancelQHYCCDExposing, CancelQHYCCDExposingAndReadout, CloseQHYCCD,
    ExpQHYCCDSingleFrame, GetQHYCCDChipInfo, GetQHYCCDEffectiveArea, GetQHYCCDExposureRemaining,
    GetQHYCCDFWVersion, GetQHYCCDLiveFrame, GetQHYCCDMemLength, GetQHYCCDModel,
    GetQHYCCDNumberOfReadModes, GetQHYCCDOverScanArea, GetQHYCCDParam, GetQHYCCDParamMinMaxStep,
    GetQHYCCDReadMode, GetQHYCCDReadModeName, GetQHYCCDReadModeResolution, GetQHYCCDSingleFrame,
    GetQHYCCDType, InitQHYCCD, IsQHYCCDCFWPlugged, IsQHYCCDControlAvailable, OpenQHYCCD,
    SetQHYCCDBinMode, SetQHYCCDBitsMode, SetQHYCCDDebayerOnOff, SetQHYCCDParam, SetQHYCCDReadMode,
    SetQHYCCDResolution, SetQHYCCDStreamMode, StopQHYCCDLive, QHYCCD_ERROR, QHYCCD_ERROR_F64,
    QHYCCD_READ_DIRECTLY, QHYCCD_SUCCESS,
};

#[cfg(feature = "simulation")]
use crate::simulation::{self, SimulatedCameraState};

// --- filter-wheel position <-> CONTROL_CFWPORT ASCII encoding ------------------
//
// A QHY CFW addresses slots by a single HEX ASCII digit carried on
// `CONTROL_CFWPORT`: slot 0 -> '0' (0x30) .. slot 9 -> '9', slot 10 -> 'A' (0x41)
// .. slot 15 -> 'F' (0x46), up to the SDK's 16 positions. (QHYCCD Filter Wheel
// API manual: '0' == position 1 .. 'F' == position 16; INDI's indi-qhy encodes
// `position - 1` with `"%X"` and decodes with `strtol(.., 16)`.) For slots 0-9
// hex and decimal coincide, so this differs from a plain `+48` decimal offset
// only from slot 10 up — where a decimal offset would command/report the WRONG
// physical slot. Shared by the real accessors and the simulation backend so a
// crate round-trip stays consistent; verified only on hardware for slots >= 10.

/// Slots a `CONTROL_CFWPORT` value can address. This is the encoding's bound,
/// not a chosen limit — one hex digit names sixteen positions and no more — so
/// anything advertising a slot *count* has to agree with it (a test below pins
/// the two together).
///
/// Only a slot *count* needs this; the encoders carry the bound in their own
/// signatures. That is the simulator's config and the test, so the const is
/// scoped to them rather than sitting dead in a real-SDK build.
#[cfg(any(feature = "simulation", test))]
pub(crate) const CFW_MAX_SLOTS: u32 = 16;

/// Encode a 0-indexed slot as its `CONTROL_CFWPORT` hex-ASCII code, or `None`
/// for a slot the encoding cannot name. A single hex digit addresses sixteen
/// positions and no more, which is what [`char::from_digit`] enforces here —
/// the bound lives in the return type rather than in this sentence.
pub(crate) fn cfw_slot_to_ascii(slot: u32) -> Option<u32> {
    char::from_digit(slot, 16).map(|d| u32::from(d.to_ascii_uppercase()))
}

/// Decode a `CONTROL_CFWPORT` hex-ASCII code back to a 0-indexed slot.
///
/// [`char::to_digit`] is **ASCII-only**: std defines a digit as `0-9`, `a-z`,
/// `A-Z`, so at radix 16 it accepts exactly `0-9`, `A-F`, `a-f` — the codes the
/// wheel can report, in either case — and nothing else, non-ASCII code points
/// included. Any other byte falls back to the legacy decimal decode so a
/// nonstandard value degrades rather than panics.
fn cfw_ascii_to_slot(ascii: u32) -> u32 {
    char::from_u32(ascii)
        .and_then(|c| c.to_digit(16))
        .unwrap_or_else(|| ascii.saturating_sub(0x30))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod cfw_encoding_tests {
    use super::{cfw_ascii_to_slot, cfw_slot_to_ascii};

    #[test]
    fn slot_to_ascii_uses_hex_digits() {
        assert_eq!(cfw_slot_to_ascii(0), Some(u32::from(b'0'))); // 48
        assert_eq!(cfw_slot_to_ascii(9), Some(u32::from(b'9'))); // 57
        assert_eq!(cfw_slot_to_ascii(10), Some(u32::from(b'A'))); // 65, NOT ':' (58)
        assert_eq!(cfw_slot_to_ascii(15), Some(u32::from(b'F'))); // 70
    }

    #[test]
    fn slot_to_ascii_has_no_code_past_the_sixteenth_slot() {
        // The SDK addresses the wheel with one hex digit, so slot 16 is not a
        // slot with an unusual code — it is a slot with no code at all.
        assert_eq!(cfw_slot_to_ascii(16), None);
        assert_eq!(cfw_slot_to_ascii(u32::MAX), None);
    }

    #[test]
    fn max_slots_is_exactly_where_the_encoding_runs_out() {
        // `CFW_MAX_SLOTS` is advertised as a slot count elsewhere (the
        // simulator caps its wheel with it), so it has to stay pinned to the
        // codec rather than drift into a second, independent 16.
        assert!(cfw_slot_to_ascii(super::CFW_MAX_SLOTS - 1).is_some());
        assert_eq!(cfw_slot_to_ascii(super::CFW_MAX_SLOTS), None);
    }

    #[test]
    fn ascii_to_slot_decodes_hex_including_lowercase() {
        assert_eq!(cfw_ascii_to_slot(u32::from(b'0')), 0);
        assert_eq!(cfw_ascii_to_slot(u32::from(b'9')), 9);
        assert_eq!(cfw_ascii_to_slot(u32::from(b'A')), 10);
        assert_eq!(cfw_ascii_to_slot(u32::from(b'f')), 15);
    }

    #[test]
    fn encoding_round_trips_every_slot_0_to_15() {
        for slot in 0..16 {
            let ascii = cfw_slot_to_ascii(slot).unwrap();
            assert_eq!(cfw_ascii_to_slot(ascii), slot);
        }
    }

    #[test]
    fn slots_0_through_9_match_the_legacy_decimal_offset() {
        // Hex and decimal `+48` coincide below 10; they diverge only from slot 10.
        for slot in 0..10 {
            assert_eq!(cfw_slot_to_ascii(slot), Some(slot + 48));
        }
    }

    #[test]
    fn ascii_to_slot_falls_back_to_the_decimal_decode_off_the_hex_range() {
        // Not hex digits, so the fallback carries them: the wheel reporting
        // something nonstandard degrades to a number rather than panicking.
        assert_eq!(cfw_ascii_to_slot(u32::from(b'G')), 23); // 0x47 - 0x30
        assert_eq!(cfw_ascii_to_slot(0), 0); // saturates rather than wrapping
    }
}

// --- ControlType: the SDK CONTROL_ID address space ----------------------------

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
/// A QHYCCD control (`CONTROL_ID`) addressed by `is_control_available`,
/// `get_parameter`, `set_parameter`, and `get_parameter_min_max_step`.
///
/// Only the controls this workspace actually touches are named; every other
/// SDK `CONTROL_ID` is preserved as [`ControlType::Other`] carrying its raw
/// value. This mirrors the `zwo-rs` / `svbony-rs` `ControlType` shape (a small
/// semantic subset plus an `Other` escape) rather than transcribing the SDK's
/// full ~90-entry enum. Documentation is taken from the QHYCCD SDK
/// (<https://www.qhyccd.cn/file/repository/publish/SDK/code/QHYCCD%20SDK_API_EN_V2.3.pdf>).
///
/// The named variants are no longer `#[repr]`-discriminated (a data-carrying
/// enum cannot be `as u32`-cast); use [`ControlType::to_raw`] to obtain the SDK
/// `CONTROL_ID`, whose numeric values still match the SDK's own numbering
/// (`Gain == 6`, `CfwPort == 17`, …).
pub enum ControlType {
    /// `CONTROL_BRIGHTNESS` (0) — image brightness.
    Brightness,
    /// `CONTROL_WBR` (2) — red white balance.
    Wbr,
    /// `CONTROL_WBB` (3) — blue white balance.
    Wbb,
    /// `CONTROL_WBG` (4) — green white balance.
    Wbg,
    /// `CONTROL_GAIN` (6) — sensor gain.
    Gain,
    /// `CONTROL_OFFSET` (7) — sensor offset (black level).
    Offset,
    /// `CONTROL_EXPOSURE` (8) — exposure time in microseconds.
    Exposure,
    /// `CONTROL_SPEED` (9) — readout speed.
    Speed,
    /// `CONTROL_TRANSFERBIT` (10) — USB transfer bit depth (8 or 16).
    TransferBit,
    /// `CONTROL_USBTRAFFIC` (12) — USB traffic / bandwidth control.
    UsbTraffic,
    /// `CONTROL_CURTEMP` (14) — current sensor temperature (°C).
    CurTemp,
    /// `CONTROL_CURPWM` (15) — current cooler PWM (0–255).
    CurPWM,
    /// `CONTROL_MANULPWM` (16) — manual cooler PWM set-point (0–255).
    ManualPWM,
    /// `CONTROL_CFWPORT` (17) — filter-wheel position (ASCII-offset value).
    CfwPort,
    /// `CONTROL_COOLER` (18) — auto-cooling target temperature (°C).
    Cooler,
    /// `CONTROL_CAM_COLOR` (20) — Bayer matrix / colour support.
    CamColor,
    /// `CAM_BIN1X1MODE` (21) — 1×1 binning support.
    CamBin1x1mode,
    /// `CAM_BIN2X2MODE` (22) — 2×2 binning support.
    CamBin2x2mode,
    /// `CAM_BIN3X3MODE` (23) — 3×3 binning support.
    CamBin3x3mode,
    /// `CAM_BIN4X4MODE` (24) — 4×4 binning support.
    CamBin4x4mode,
    /// `CAM_MECHANICALSHUTTER` (25) — mechanical-shutter presence.
    CamMechanicalShutter,
    /// `CAM_8BITS` (34) — 8-bit image output support.
    Cam8bits,
    /// `CAM_16BITS` (35) — 16-bit image output support.
    Cam16bits,
    /// `CONTROL_CFWSLOTSNUM` (44) — number of filter-wheel slots.
    CfwSlotsNum,
    /// `CONTROL_DDR` (48) — DDR frame buffer support (live-mode capture).
    DDR,
    /// `OutputDataActualBits` (55) — actual bit depth of the output data.
    OutputDataActualBits,
    /// `CAM_SINGLEFRAMEMODE` (57) — single-frame capture support.
    CamSingleFrameMode,
    /// `CAM_LIVEVIDEOMODE` (58) — live-video capture support.
    CamLiveVideoMode,
    /// `CAM_IS_COLOR` (59) — colour-sensor flag.
    CamIsColor,
    /// `CAM_BIN6X6MODE` (75) — 6×6 binning support.
    CamBin6x6mode,
    /// `CAM_BIN8X8MODE` (76) — 8×8 binning support.
    CamBin8x8mode,
    /// Any control outside the subset named above; carries the raw SDK
    /// `CONTROL_ID`, in the width the SDK's own `CONTROL_ID` parameter takes.
    Other(u32),
}

impl ControlType {
    /// The SDK `CONTROL_ID` for this control. The numeric values match the
    /// QHYCCD SDK's own numbering (forced fact #3 of the convention plan).
    ///
    /// Only the real FFI arms (compiled without `simulation`) and the round-trip
    /// test consume this — the simulated backend keys controls by `ControlType`
    /// directly — so it is dead code on a `simulation` non-test build.
    #[must_use]
    #[cfg_attr(feature = "simulation", allow(dead_code))]
    pub(crate) fn to_raw(self) -> u32 {
        match self {
            Self::Brightness => 0,
            Self::Wbr => 2,
            Self::Wbb => 3,
            Self::Wbg => 4,
            Self::Gain => 6,
            Self::Offset => 7,
            Self::Exposure => 8,
            Self::Speed => 9,
            Self::TransferBit => 10,
            Self::UsbTraffic => 12,
            Self::CurTemp => 14,
            Self::CurPWM => 15,
            Self::ManualPWM => 16,
            Self::CfwPort => 17,
            Self::Cooler => 18,
            Self::CamColor => 20,
            Self::CamBin1x1mode => 21,
            Self::CamBin2x2mode => 22,
            Self::CamBin3x3mode => 23,
            Self::CamBin4x4mode => 24,
            Self::CamMechanicalShutter => 25,
            Self::Cam8bits => 34,
            Self::Cam16bits => 35,
            Self::CfwSlotsNum => 44,
            Self::DDR => 48,
            Self::OutputDataActualBits => 55,
            Self::CamSingleFrameMode => 57,
            Self::CamLiveVideoMode => 58,
            Self::CamIsColor => 59,
            Self::CamBin6x6mode => 75,
            Self::CamBin8x8mode => 76,
            Self::Other(v) => v,
        }
    }

    /// The [`ControlType`] for an SDK `CONTROL_ID` — the inverse of
    /// [`Self::to_raw`]. Unnamed ids round-trip through [`ControlType::Other`].
    /// Only the round-trip test consumes this today (no runtime path converts a
    /// raw id back to a `ControlType`), so it is test-only.
    #[cfg(test)]
    #[must_use]
    fn from_raw(v: u32) -> Self {
        match v {
            0 => Self::Brightness,
            2 => Self::Wbr,
            3 => Self::Wbb,
            4 => Self::Wbg,
            6 => Self::Gain,
            7 => Self::Offset,
            8 => Self::Exposure,
            9 => Self::Speed,
            10 => Self::TransferBit,
            12 => Self::UsbTraffic,
            14 => Self::CurTemp,
            15 => Self::CurPWM,
            16 => Self::ManualPWM,
            17 => Self::CfwPort,
            18 => Self::Cooler,
            20 => Self::CamColor,
            21 => Self::CamBin1x1mode,
            22 => Self::CamBin2x2mode,
            23 => Self::CamBin3x3mode,
            24 => Self::CamBin4x4mode,
            25 => Self::CamMechanicalShutter,
            34 => Self::Cam8bits,
            35 => Self::Cam16bits,
            44 => Self::CfwSlotsNum,
            48 => Self::DDR,
            55 => Self::OutputDataActualBits,
            57 => Self::CamSingleFrameMode,
            58 => Self::CamLiveVideoMode,
            59 => Self::CamIsColor,
            75 => Self::CamBin6x6mode,
            76 => Self::CamBin8x8mode,
            other => Self::Other(other),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod control_type_tests {
    use super::ControlType;

    /// Every named variant round-trips through its raw SDK `CONTROL_ID`, so the
    /// two hand-written `to_raw`/`from_raw` matches can never silently drift.
    #[test]
    fn named_variants_round_trip() {
        let named = [
            ControlType::Brightness,
            ControlType::Wbr,
            ControlType::Wbb,
            ControlType::Wbg,
            ControlType::Gain,
            ControlType::Offset,
            ControlType::Exposure,
            ControlType::Speed,
            ControlType::TransferBit,
            ControlType::UsbTraffic,
            ControlType::CurTemp,
            ControlType::CurPWM,
            ControlType::ManualPWM,
            ControlType::CfwPort,
            ControlType::Cooler,
            ControlType::CamColor,
            ControlType::CamBin1x1mode,
            ControlType::CamBin2x2mode,
            ControlType::CamBin3x3mode,
            ControlType::CamBin4x4mode,
            ControlType::CamMechanicalShutter,
            ControlType::Cam8bits,
            ControlType::Cam16bits,
            ControlType::CfwSlotsNum,
            ControlType::DDR,
            ControlType::OutputDataActualBits,
            ControlType::CamSingleFrameMode,
            ControlType::CamLiveVideoMode,
            ControlType::CamIsColor,
            ControlType::CamBin6x6mode,
            ControlType::CamBin8x8mode,
        ];
        for control in named {
            assert_eq!(ControlType::from_raw(control.to_raw()), control);
        }
    }

    /// A control id outside the named subset survives as `Other`, and the key
    /// SDK ids keep their documented numeric values.
    #[test]
    fn unnamed_id_survives_as_other() {
        assert_eq!(ControlType::from_raw(1), ControlType::Other(1));
        assert_eq!(ControlType::Other(1).to_raw(), 1);
        assert_eq!(ControlType::Gain.to_raw(), 6);
        assert_eq!(ControlType::CfwPort.to_raw(), 17);
        assert_eq!(ControlType::CamBin8x8mode.to_raw(), 76);
    }
}

// --- real-hardware handle machinery (compiled only without `simulation`) ------
//
// A simulated camera holds a `SimulatedCameraState` instead of a `HandleCell`,
// and every camera method forks at compile time on the feature (see the sibling
// `zwo-rs` / `svbony-rs` crates). These items therefore never touch the
// simulation types and always call the real `libqhyccd-sys` FFI. They were
// previously the separate `backend.rs` module.

// The handle cell below is compiled into the **test** build of the simulation
// variant as well as into every real build. The crate's own tests build the
// simulated library `--cfg test` (there is no real-arm test target — linking one
// would have `bazel coverage` instrument every compiled-out FFI arm as
// never-executed), and the ownership rule this cell carries is the one part of
// the real arm that can be tested without hardware. Only the FFI `Drop` stays
// real-arm-only, so a test cell holds a pointer nothing ever dereferences.
#[cfg(any(not(feature = "simulation"), test))]
#[derive(Debug, PartialEq, Copy, Clone)]
pub(crate) struct QHYCCDHandle {
    pub ptr: *const std::ffi::c_void,
}

// SAFETY: the struct holds a raw pointer (`*const c_void`), which makes it
// `!Send + !Sync` by default — so these impls are REQUIRED for `Camera`
// (whose real backend is `Arc<HandleCell>`) to be `Send + Sync`, which it must be
// to move across the async runtime / blocking threads. The pointer is an opaque
// QHYCCD SDK handle that is never dereferenced in Rust.
//
// What makes sharing it sound is that the handle is unreachable outside
// [`HandleCell::with_handle`], which holds the cell's read lock across the FFI
// call, while `open` / `close` take the write lock. An SDK call therefore cannot
// overlap the `CloseQHYCCD` that frees the handle it is using, whatever the
// driver above does — the guarantee the sibling `zwo-rs` / `svbony-rs` backends
// get from holding their handle mutex across the call.
//
// Two non-close calls on one handle still run concurrently, deliberately: that
// is what QHY drivers do in practice (INDI's `indi-qhy` polls
// `GetQHYCCDParam(CONTROL_CURTEMP)` from its event-loop timer while
// `GetQHYCCDSingleFrame` blocks on its imaging thread, holding no lock), and the
// SDK manual takes no position on it. Excluding a close is the property nobody
// gets for free; indi-qhy buys it with a `pthread_join` before `CloseQHYCCD`.
#[cfg(any(not(feature = "simulation"), test))]
unsafe impl Send for QHYCCDHandle {}
#[cfg(any(not(feature = "simulation"), test))]
unsafe impl Sync for QHYCCDHandle {}

/// RAII owner of one open QHYCCD device handle. Wraps the `RwLock<Option<..>>`
/// cell so a [`Drop`] can close the device when the last strong reference is
/// released — the zwo/svbony convention (`Camera: Drop`), which this crate
/// previously lacked (a dropped-open camera leaked the handle). The cell is
/// shared as `Arc<HandleCell>` by a [`Camera`] and its clones — including the
/// filter wheel, which a QHY CFW is driven through the *same* camera handle — so
/// `CloseQHYCCD` runs exactly once, on the last clone's drop.
///
/// `Drop` closes only an *open* handle (`Some`), so an explicit
/// [`Camera::close`] (which `take()`s the `Option`) makes it a no-op.
#[cfg(any(not(feature = "simulation"), test))]
#[derive(Debug)]
pub(crate) struct HandleCell {
    inner: RwLock<Option<QHYCCDHandle>>,
}

#[cfg(any(not(feature = "simulation"), test))]
impl HandleCell {
    /// A fresh, unopened handle cell.
    pub(crate) fn new() -> Self {
        Self {
            inner: RwLock::new(None),
        }
    }

    /// Run one SDK call on the open handle, holding the cell's read lock for
    /// exactly as long as the call.
    ///
    /// This is the crate's only route to the handle, and it takes a closure
    /// rather than returning the pointer so that the pointer cannot outlive the
    /// guard. `close` takes the write lock, so it waits for a call in flight
    /// instead of running `CloseQHYCCD` while another thread is inside the SDK
    /// with that pointer — which would free the device beneath libusb, an error
    /// libusb reports as a `usbi_mutex_lock` assertion and which can corrupt the
    /// context every other QHY device on the bus shares.
    ///
    /// Two calls do not wait for each other — they are both read guards — so a
    /// temperature poll runs during a readout, as `indi-qhy` also does.
    ///
    /// `parking_lot::RwLock` cannot be poisoned, so the only failure is an
    /// unopened handle (`None`), reported as [`QHYError::CameraNotOpen`] to
    /// match the simulation backend. `f` does not run in that case.
    pub(crate) fn with_handle<T>(&self, f: impl FnOnce(*const std::ffi::c_void) -> T) -> Result<T> {
        // Bound to a name rather than left as a `match` scrutinee temporary:
        // both hold the guard across the call, but only this one says so.
        let cell = self.inner.read();
        match *cell {
            Some(handle) => Ok(f(handle.ptr)),
            None => {
                tracing::error!(error = ?QHYError::CameraNotOpen);
                Err(QHYError::CameraNotOpen)
            }
        }
    }
}

// Transparent access to the underlying lock so the `handle.read()` /
// `handle.write()` call sites in `open` / `close` / `is_open` keep working
// unchanged. Every *use* of the handle goes through `with_handle` instead.
#[cfg(any(not(feature = "simulation"), test))]
impl std::ops::Deref for HandleCell {
    type Target = RwLock<Option<QHYCCDHandle>>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[cfg(not(feature = "simulation"))]
impl Drop for HandleCell {
    fn drop(&mut self) {
        // Best-effort last-drop close. `get_mut()` is contention-free — we hold
        // the only strong reference at drop. Closing a `None` handle is skipped,
        // so a prior explicit `close()` (or `Sdk::drop`'s pre-release close)
        // makes this a no-op rather than a double-close.
        if let Some(handle) = self.inner.get_mut().take() {
            match unsafe { crate::sys::CloseQHYCCD(handle.ptr) } {
                crate::sys::QHYCCD_SUCCESS => {}
                _ => {
                    tracing::error!(
                        error = ?crate::QHYError::Sdk { op: "close_camera" },
                        "failed to close camera handle on drop"
                    );
                }
            }
        }
    }
}

// --- the Camera device --------------------------------------------------------

/// The representation of a camera. It is constructed by the SDK and can be used to
/// interact with the camera.
///
/// The real/simulated backend is chosen at **compile time** by the `simulation`
/// feature (matching the sibling `zwo-rs` / `svbony-rs` crates): without it a
/// `Camera` owns a shared FFI [`HandleCell`]; with it, a shared
/// `SimulatedCameraState`. Both are shared via `Arc` so a `Camera` stays `Clone`
/// and its filter wheel drives the *same* device. Two cameras are equal when they
/// share an `id`, regardless of the live backend.
#[derive(Debug, Clone)]
pub struct Camera {
    id: String,
    /// Real backend (compiled without `simulation`): the shared open-handle cell.
    /// Closed on the last clone's drop (see [`HandleCell`]).
    #[cfg(not(feature = "simulation"))]
    handle: Arc<HandleCell>,
    /// Simulated backend (compiled with `simulation`): the shared mutable device
    /// state, so a `Camera` clone and its filter wheel act on one simulated device.
    #[cfg(feature = "simulation")]
    state: Arc<RwLock<SimulatedCameraState>>,
}

impl PartialEq for Camera {
    /// Two cameras are equal when they share an `id`, regardless of the live
    /// backend handle / simulated state (id-only equality — the former
    /// `#[partial_eq(skip)]` on the backend field).
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Camera {
    /// Creates a new instance of the camera. The Sdk automatically finds all cameras and provides them in it's cameras() iterator. Creating
    /// a camera manually should only be needed for rare cases.
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk, Camera};
    /// let camera = Camera::new("camera id from sdk".to_string());
    /// println!("Camera: {:?}", camera);
    /// ```
    #[cfg(not(feature = "simulation"))]
    pub fn new(id: String) -> Self {
        Self {
            id,
            handle: Arc::new(HandleCell::new()),
        }
    }

    /// Creates a new simulated camera instance
    ///
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::Camera;
    /// use qhyccd_rs::simulation::SimulatedCameraConfig;
    ///
    /// let config = SimulatedCameraConfig::default()
    ///     .with_filter_wheel(5)
    ///     .with_cooler();
    /// let camera = Camera::new_simulated(config);
    /// ```
    #[cfg(feature = "simulation")]
    pub fn new_simulated(config: simulation::SimulatedCameraConfig) -> Self {
        let id = config.id.clone();
        Self {
            id,
            state: Arc::new(RwLock::new(SimulatedCameraState::new(config))),
        }
    }

    /// Returns true if this is a simulated camera. Constant per build: every
    /// camera is simulated with the `simulation` feature, and none without it.
    #[cfg(feature = "simulation")]
    pub fn is_simulated(&self) -> bool {
        true
    }

    /// Returns true if this is a simulated camera (always false without the
    /// `simulation` feature).
    #[cfg(not(feature = "simulation"))]
    pub fn is_simulated(&self) -> bool {
        false
    }

    /// Returns the id of the camera
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::Sdk;
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// println!("Camera id: {}", camera.id());
    /// ```
    pub fn id(&self) -> &str {
        self.id.as_str()
    }
}

// --- lifecycle: open / close / init / is_open ------------------------------
impl Camera {
    /// Opens a camera with the given id. The SDK automatically finds all connected cameras upon initialization
    /// but does not call open on the cameras. You have to call open on the camera you want to use. Calling open
    /// on a camera that is already open does not do anything.
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// ```
    pub fn open(&self) -> Result<()> {
        if self.is_open()? {
            return Ok(());
        }
        #[cfg(not(feature = "simulation"))]
        {
            // read and see if the handle is already Some(_)
            let mut lock = self.handle.write();
            unsafe {
                match std::ffi::CString::new(self.id.clone()) {
                    Ok(c_id) => {
                        let handle = OpenQHYCCD(c_id.as_ptr());
                        if handle.is_null() {
                            let error = QHYError::Sdk { op: "open_camera" };
                            tracing::error!(error = ?error);
                            return Err(error);
                        }
                        *lock = Some(QHYCCDHandle { ptr: handle });
                        Ok(())
                    }
                    Err(error) => {
                        tracing::error!(error = ?error);
                        Err(error.into())
                    }
                }
            }
        }
        #[cfg(feature = "simulation")]
        {
            let mut state = self.state.write();
            state.is_open = true;
            Ok(())
        }
    }

    /// Closes the camera. If you have to call this function, you can then open the camera again by
    /// calling `open`. Calling close on a camera that is not open does not do anything.
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// camera.close().expect("close failed");
    /// ```
    pub fn close(&self) -> Result<()> {
        if !self.is_open()? {
            return Ok(());
        }
        #[cfg(not(feature = "simulation"))]
        {
            let mut lock = self.handle.write();

            match *lock {
                Some(handle) => {
                    check(unsafe { CloseQHYCCD(handle.ptr) }, "close_camera")?;
                    lock.take();
                    Ok(())
                }
                None => Ok(()),
            }
        }
        #[cfg(feature = "simulation")]
        {
            let mut state = self.state.write();
            state.is_open = false;
            state.is_initialized = false;
            // Real `CloseQHYCCD` destroys the device handle; a later `open()`
            // yields a fresh device with NO retained session state. Clear the
            // per-session imaging state so a `close()` -> `open()` cycle presents a
            // clean device — otherwise a stale `live_mode_active` / `captured_image`
            // / `exposure_start` makes `get_live_frame` / `get_single_frame`
            // falsely succeed with no intervening `begin_live()` / exposure, a
            // false pass real hardware can never produce.
            state.live_mode_active = false;
            state.captured_image = None;
            state.captured_image_metadata = None;
            state.exposure_start = None;
            state.stream_mode = None;
            Ok(())
        }
    }

    /// initializes the camera to a new session - use this to change from LiveMode to SingleFrameMode for instance
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk, StreamMode};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// camera.set_stream_mode(StreamMode::LiveMode).expect("set_stream_mode failed");
    /// camera.init().expect("init failed");
    /// ```
    pub fn init(&self) -> Result<()> {
        #[cfg(not(feature = "simulation"))]
        {
            check(
                self.handle
                    .with_handle(|handle| unsafe { InitQHYCCD(handle) })?,
                "init_camera",
            )
        }
        #[cfg(feature = "simulation")]
        {
            let mut state = self.state.write();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            state.is_initialized = true;
            // Reset ROI to full frame based on current readout mode
            // The mode index is an SDK `u32` and the mode table is a `Vec`. An
            // index this target cannot address falls back to the full chip,
            // exactly as an out-of-range one already does.
            let mode_index = usize::try_from(state.readout_mode).ok();
            let (width, height) = mode_index
                .and_then(|i| state.config.readout_modes.get(i))
                .map(|(_, res)| *res)
                .unwrap_or((
                    state.config.chip_info.image_width,
                    state.config.chip_info.image_height,
                ));
            state.roi = CCDChipArea {
                start_x: 0,
                start_y: 0,
                width,
                height,
            };
            Ok(())
        }
    }

    /// Returns `true` if the camera is open
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found"); // this does not open the camera
    /// camera.open().expect("open failed");
    /// let is_open = camera.is_open();
    /// println!("Is camera open: {:?}", is_open);
    /// ```
    pub fn is_open(&self) -> Result<bool> {
        #[cfg(not(feature = "simulation"))]
        {
            let lock = self.handle.read();
            Ok((*lock).is_some())
        }
        #[cfg(feature = "simulation")]
        {
            let state = self.state.read();
            Ok(state.is_open)
        }
    }
}

// --- configuration: stream mode / readout mode / binning / debayer / ROI / bit mode
impl Camera {
    /// Sets the stream mode of the camera
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera,StreamMode};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// camera.set_stream_mode(StreamMode::LiveMode).expect("set_stream_mode failed");
    /// ```
    pub fn set_stream_mode(&self, mode: StreamMode) -> Result<()> {
        #[cfg(not(feature = "simulation"))]
        {
            check(
                self.handle
                    .with_handle(|handle| unsafe { SetQHYCCDStreamMode(handle, mode as u8) })?,
                "set_stream_mode",
            )
        }
        #[cfg(feature = "simulation")]
        {
            let mut state = self.state.write();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            state.stream_mode = Some(mode);
            Ok(())
        }
    }

    /// Sets the readout mode of the camera with the id of the `ReadoutMode` between 0 and the value
    /// returned by `get_number_of_readout_modes`
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// camera.set_readout_mode(0).expect("set_readout_mode failed");
    /// ```
    pub fn set_readout_mode(&self, mode: u32) -> Result<()> {
        #[cfg(not(feature = "simulation"))]
        {
            check(
                self.handle
                    .with_handle(|handle| unsafe { SetQHYCCDReadMode(handle, mode) })?,
                "set_readout_mode",
            )
        }
        #[cfg(feature = "simulation")]
        {
            let mut state = self.state.write();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            // An index this target cannot address is out of range by definition.
            if !usize::try_from(mode).is_ok_and(|i| i < state.config.readout_modes.len()) {
                return Err(QHYError::Sdk {
                    op: "set_readout_mode",
                });
            }
            state.readout_mode = mode;
            Ok(())
        }
    }

    /// Sets the binning mode of the camera
    /// Only symmetric binnings are supported
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// camera.set_bin_mode(1, 1).expect("set_bin_mode failed");
    /// ```
    pub fn set_bin_mode(&self, bin_x: u32, bin_y: u32) -> Result<()> {
        #[cfg(not(feature = "simulation"))]
        {
            check(
                self.handle
                    .with_handle(|handle| unsafe { SetQHYCCDBinMode(handle, bin_x, bin_y) })?,
                "set_bin_mode",
            )
        }
        #[cfg(feature = "simulation")]
        {
            let mut state = self.state.write();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            state.binning = (bin_x, bin_y);
            Ok(())
        }
    }

    /// According to c-cod ethis does not work for all cameras
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// camera.set_debayer(true).expect("set_debayer failed");
    /// ```
    pub fn set_debayer(&self, on: bool) -> Result<()> {
        #[cfg(not(feature = "simulation"))]
        {
            check(
                self.handle
                    .with_handle(|handle| unsafe { SetQHYCCDDebayerOnOff(handle, on) })?,
                "set_debayer",
            )
        }
        #[cfg(feature = "simulation")]
        {
            let mut state = self.state.write();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            state.debayer_enabled = on;
            Ok(())
        }
    }

    /// Sets the Region of interest of the camera
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera, CCDChipArea};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// let roi = CCDChipArea {
    ///     start_x: 0,
    ///     start_y: 0,
    ///     width: 640,
    ///     height: 480,
    /// };
    /// camera.set_roi(roi).expect("set_roi failed");
    /// ```
    pub fn set_roi(&self, roi: CCDChipArea) -> Result<()> {
        #[cfg(not(feature = "simulation"))]
        {
            check(
                self.handle.with_handle(|handle| unsafe {
                    SetQHYCCDResolution(handle, roi.start_x, roi.start_y, roi.width, roi.height)
                })?,
                "set_roi",
            )
        }
        #[cfg(feature = "simulation")]
        {
            let mut state = self.state.write();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            state.roi = roi;
            Ok(())
        }
    }

    /// Sets the USB transfer mode to either 8 or 16 bit
    ///
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// camera.set_bit_mode(16).expect("set_bit_mode failed");
    /// ```
    pub fn set_bit_mode(&self, mode: u32) -> Result<()> {
        #[cfg(not(feature = "simulation"))]
        {
            check(
                self.handle
                    .with_handle(|handle| unsafe { SetQHYCCDBitsMode(handle, mode) })?,
                "set_bit_mode",
            )
        }
        #[cfg(feature = "simulation")]
        {
            let mut state = self.state.write();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            state.bit_depth = mode;
            Ok(())
        }
    }
}

// --- device info: model / firmware / type / chip info / overscan / effective area
impl Camera {
    /// Returns the model of the camera
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// let model = camera.get_model().expect("get_model failed");
    /// println!("Camera model: {}", model);
    /// ```
    pub fn get_model(&self) -> Result<String> {
        #[cfg(not(feature = "simulation"))]
        {
            let mut model: [c_char; 128] = [0; 128];
            let status = self
                .handle
                .with_handle(|handle| unsafe { GetQHYCCDModel(handle, model.as_mut_ptr()) })?;
            match status {
                QHYCCD_SUCCESS => {
                    // Bounded decode: the SDK documents no maximum length and no NUL
                    // guarantee, so scan for the terminator WITHIN the fixed 128-byte
                    // buffer rather than with an unbounded `CStr::from_ptr` (which
                    // would over-read past the array if the SDK ever filled all 128
                    // bytes without a NUL). A missing terminator becomes a clean
                    // error, not UB. 128 matches INDI's `QHYReadModeInfo::label`.
                    let bytes = unsafe {
                        std::slice::from_raw_parts(model.as_ptr().cast::<u8>(), model.len())
                    };
                    let model = CStr::from_bytes_until_nul(bytes)
                        .map_err(|_| QHYError::Sdk { op: "get_model" })?
                        .to_str()
                        .inspect_err(|error| tracing::error!(error = ?error))?;
                    Ok(model.to_string())
                }
                _ => {
                    let error = QHYError::Sdk { op: "get_model" };
                    tracing::error!(error = ?error);
                    Err(error)
                }
            }
        }
        #[cfg(feature = "simulation")]
        {
            let state = self.state.read();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            Ok(state.config.model.clone())
        }
    }

    /// Returns the firmware version of the camera
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// let firmware_version = camera.get_firmware_version().expect("get_firmware_version failed");
    /// println!("Firmware version: {}", firmware_version);
    /// ```
    pub fn get_firmware_version(&self) -> Result<String> {
        #[cfg(not(feature = "simulation"))]
        {
            let mut version = [0u8; 32];
            let status = self.handle.with_handle(|handle| unsafe {
                GetQHYCCDFWVersion(handle, version.as_mut_ptr())
            })?;
            match status {
                QHYCCD_SUCCESS => {
                    if version[0] >> 4 <= 9 {
                        Ok(format!(
                            "Firmware version: 20{}_{}_{}",
                            (((version[0] >> 4) + 0x10) as u32),
                            version[0] & 0x0F,
                            version[1]
                        ))
                    } else {
                        Ok(format!(
                            "Firmware version: 20{}_{}_{}",
                            ((version[0] >> 4) as u32),
                            version[0] & 0x0F,
                            version[1]
                        ))
                    }
                }
                _ => {
                    let error = QHYError::Sdk {
                        op: "get_firmware_version",
                    };
                    tracing::error!(error = ?error);
                    Err(error)
                }
            }
        }
        #[cfg(feature = "simulation")]
        {
            let state = self.state.read();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            Ok(state.config.firmware_version.clone())
        }
    }

    /// Not sure what this does, for a QHY178M Cool it returns 4010
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// let camera_type = camera.get_type().expect("get_type failed");
    /// println!("Camera type: {}", camera_type);
    /// ```
    pub fn get_type(&self) -> Result<u32> {
        #[cfg(not(feature = "simulation"))]
        {
            let status = self
                .handle
                .with_handle(|handle| unsafe { GetQHYCCDType(handle) })?;
            match status {
                QHYCCD_ERROR => {
                    let error = QHYError::Sdk { op: "get_type" };
                    tracing::error!(error = ?error);
                    Err(error)
                }
                camera_type => Ok(camera_type),
            }
        }
        #[cfg(feature = "simulation")]
        {
            let state = self.state.read();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            Ok(state.config.camera_type)
        }
    }

    /// Returns the CCD chip information
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// let ccd_info = camera.get_ccd_info().expect("get_ccd_info failed");
    /// println!("CCD info: {:?}", ccd_info);
    /// ```
    pub fn get_ccd_info(&self) -> Result<CCDChipInfo> {
        #[cfg(not(feature = "simulation"))]
        {
            let mut chipw: f64 = 0.0;
            let mut chiph: f64 = 0.0;
            let mut imagew: u32 = 0;
            let mut imageh: u32 = 0;
            let mut pixelw: f64 = 0.0;
            let mut pixelh: f64 = 0.0;
            let mut bpp: u32 = 0;
            let status = self.handle.with_handle(|handle| unsafe {
                GetQHYCCDChipInfo(
                    handle,
                    &mut chipw as *mut f64,
                    &mut chiph as *mut f64,
                    &mut imagew as *mut u32,
                    &mut imageh as *mut u32,
                    &mut pixelw as *mut f64,
                    &mut pixelh as *mut f64,
                    &mut bpp as *mut u32,
                )
            })?;
            match status {
                QHYCCD_SUCCESS => Ok(CCDChipInfo {
                    chip_width: chipw,
                    chip_height: chiph,
                    image_width: imagew,
                    image_height: imageh,
                    pixel_width: pixelw,
                    pixel_height: pixelh,
                    bits_per_pixel: bpp,
                }),
                _ => {
                    let error = QHYError::Sdk { op: "get_ccd_info" };
                    tracing::error!(error = ?error);
                    Err(error)
                }
            }
        }
        #[cfg(feature = "simulation")]
        {
            let state = self.state.read();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            Ok(state.config.chip_info)
        }
    }

    /// Get the overscan area of the chip
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// let overscan_area = camera.get_overscan_area().expect("get_overscan_area failed");
    /// println!("Overscan area: {:?}", overscan_area);
    /// ```
    pub fn get_overscan_area(&self) -> Result<CCDChipArea> {
        #[cfg(not(feature = "simulation"))]
        {
            let mut start_x: u32 = 0;
            let mut start_y: u32 = 0;
            let mut width: u32 = 0;
            let mut height: u32 = 0;
            let status = self.handle.with_handle(|handle| unsafe {
                GetQHYCCDOverScanArea(
                    handle,
                    &mut start_x as *mut u32,
                    &mut start_y as *mut u32,
                    &mut width as *mut u32,
                    &mut height as *mut u32,
                )
            })?;
            match status {
                QHYCCD_SUCCESS => Ok(CCDChipArea {
                    start_x,
                    start_y,
                    width,
                    height,
                }),
                _ => {
                    let error = QHYError::Sdk {
                        op: "get_overscan_area",
                    };
                    tracing::error!(error = ?error);
                    Err(error)
                }
            }
        }
        #[cfg(feature = "simulation")]
        {
            let state = self.state.read();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            Ok(state.config.overscan_area)
        }
    }

    /// Get the effective imaging chip area
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// let effective_area = camera.get_effective_area().expect("get_effective_area failed");
    /// println!("Effective area: {:?}", effective_area);
    /// ```
    pub fn get_effective_area(&self) -> Result<CCDChipArea> {
        #[cfg(not(feature = "simulation"))]
        {
            let mut start_x: u32 = 0;
            let mut start_y: u32 = 0;
            let mut width: u32 = 0;
            let mut height: u32 = 0;
            let status = self.handle.with_handle(|handle| unsafe {
                GetQHYCCDEffectiveArea(
                    handle,
                    &mut start_x as *mut u32,
                    &mut start_y as *mut u32,
                    &mut width as *mut u32,
                    &mut height as *mut u32,
                )
            })?;
            match status {
                QHYCCD_SUCCESS => Ok(CCDChipArea {
                    start_x,
                    start_y,
                    width,
                    height,
                }),
                _ => {
                    let error = QHYError::Sdk {
                        op: "get_effective_area",
                    };
                    tracing::error!(error = ?error);
                    Err(error)
                }
            }
        }
        #[cfg(feature = "simulation")]
        {
            let state = self.state.read();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            Ok(state.config.effective_area)
        }
    }
}

// --- imaging: live + single-frame capture and exposure control -------------
impl Camera {
    /// Starts Live Video Mode on the camera
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera,StreamMode};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// camera.set_stream_mode(StreamMode::LiveMode).expect("set_stream_mode failed");
    /// camera.init().expect("init failed");
    /// camera.begin_live().expect("begin_live failed");
    /// ```
    pub fn begin_live(&self) -> Result<()> {
        #[cfg(not(feature = "simulation"))]
        {
            check(
                self.handle
                    .with_handle(|handle| unsafe { BeginQHYCCDLive(handle) })?,
                "begin_live",
            )
        }
        #[cfg(feature = "simulation")]
        {
            let mut state = self.state.write();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            state.live_mode_active = true;
            Ok(())
        }
    }

    /// Stops Live Video Mode on the camera
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera,StreamMode};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// camera.set_stream_mode(StreamMode::LiveMode).expect("set_stream_mode failed");
    /// camera.init().expect("init failed");
    /// camera.begin_live().expect("begin_live failed");
    /// camera.end_live().expect("end_live failed");
    /// ```
    pub fn end_live(&self) -> Result<()> {
        #[cfg(not(feature = "simulation"))]
        {
            check(
                self.handle
                    .with_handle(|handle| unsafe { StopQHYCCDLive(handle) })?,
                "end_live",
            )
        }
        #[cfg(feature = "simulation")]
        {
            let mut state = self.state.write();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            state.live_mode_active = false;
            Ok(())
        }
    }

    /// Returns the number of bytes needed to retrieve the image stored in the camera
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// let image_size = camera.get_image_size().expect("get_image_size failed");
    /// println!("Image size: {}", image_size);
    /// ```
    pub fn get_image_size(&self) -> Result<usize> {
        #[cfg(not(feature = "simulation"))]
        {
            let status = self
                .handle
                .with_handle(|handle| unsafe { GetQHYCCDMemLength(handle) })?;
            match status {
                QHYCCD_ERROR => {
                    let error = QHYError::Sdk {
                        op: "get_image_size",
                    };
                    tracing::error!(error = ?error);
                    Err(error)
                }
                // Callers allocate this many bytes, so a length this target
                // cannot address is an SDK error rather than a saturated
                // allocation.
                size => usize::try_from(size).map_err(|_| QHYError::Sdk {
                    op: "get_image_size",
                }),
            }
        }
        #[cfg(feature = "simulation")]
        {
            let state = self.state.read();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            Ok(state.calculate_buffer_size())
        }
    }

    /// Downloads the current Live Video Mode frame into the caller-owned `buf`,
    /// returning its [`FrameInfo`] dimensions.
    ///
    /// `buf` must be at least [`get_image_size`](Camera::get_image_size) bytes
    /// for the current frame; a short buffer is rejected with
    /// [`QHYError::BufferTooSmall`] **before** the SDK is called (the FFI would
    /// otherwise write out of bounds). This is the caller-owned-buffer convention
    /// of `zwo-rs` / `svbony-rs`, replacing the former fresh-`Vec`-per-frame
    /// return.
    ///
    /// # Live-mode polling
    ///
    /// In live mode a frame arrives asynchronously at the sensor frame rate.
    /// `GetQHYCCDLiveFrame` returns `QHYCCD_ERROR` — mapped here to
    /// `Err(QHYError::Sdk)` — both when **no frame is ready yet** and on a hard
    /// failure; the SDK does not distinguish the two. Callers must therefore
    /// **poll**: treat `Err` as "retry" for a bounded number of attempts (INDI's
    /// indi-qhy retries ~10× at 1 ms) before giving up. The simulation models this
    /// via
    /// [`SimulatedCameraConfig::with_live_not_ready_probability`](crate::simulation::SimulatedCameraConfig::with_live_not_ready_probability).
    /// # Example
    /// ```no_run
    /// use std::{thread, time::Duration};
    /// use qhyccd_rs::{Sdk,Camera,StreamMode};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// camera.set_stream_mode(StreamMode::LiveMode).expect("set_stream_mode failed");
    /// camera.init().expect("init failed");
    /// camera.begin_live().expect("begin_live failed");
    /// thread::sleep(Duration::from_millis(100));
    /// let buffer_size = camera.get_image_size().expect("get_image_size failed");
    /// let mut buf = vec![0u8; buffer_size];
    /// let info = camera.get_live_frame(&mut buf).expect("get_live_frame failed");
    /// println!("frame: {:?}", info);
    /// ```
    pub fn get_live_frame(&self, buf: &mut [u8]) -> Result<FrameInfo> {
        #[cfg(not(feature = "simulation"))]
        {
            let need = self.get_image_size()?;
            if buf.len() < need {
                let error = QHYError::BufferTooSmall {
                    needed: need,
                    got: buf.len(),
                };
                tracing::error!(error = ?error);
                return Err(error);
            }
            let mut width: u32 = 0;
            let mut height: u32 = 0;
            let mut bpp: u32 = 0;
            let mut channels: u32 = 0;
            let status = self.handle.with_handle(|handle| unsafe {
                GetQHYCCDLiveFrame(
                    handle,
                    &mut width as *mut u32,
                    &mut height as *mut u32,
                    &mut bpp as *mut u32,
                    &mut channels as *mut u32,
                    buf.as_mut_ptr(),
                )
            })?;
            match status {
                QHYCCD_SUCCESS => Ok(FrameInfo {
                    width,
                    height,
                    bits_per_pixel: bpp,
                    channels,
                }),
                _ => {
                    let error = QHYError::Sdk {
                        op: "get_live_frame",
                    };
                    tracing::error!(error = ?error);
                    Err(error)
                }
            }
        }
        #[cfg(feature = "simulation")]
        {
            let state = self.state.read();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            if !state.live_mode_active {
                return Err(QHYError::Sdk {
                    op: "get_live_frame",
                });
            }
            // Real GetQHYCCDLiveFrame returns the retryable QHYCCD_ERROR between
            // frames; model that with the configured probability (default 0.0 =
            // always ready) so a consumer's poll/retry loop is exercised.
            if state.roll_live_not_ready() {
                return Err(QHYError::Sdk {
                    op: "get_live_frame",
                });
            }

            let (width, height) = state.get_current_image_dimensions();
            let bpp = state.bit_depth;
            let channels = state.get_channels();

            let generator = simulation::ImageGenerator::default();
            let data = if bpp <= 8 {
                generator.generate_8bit(width, height, channels)
            } else {
                generator.generate_16bit(width, height, channels)
            };

            // The bound check and the slice are the same statement, so they
            // cannot drift apart.
            let Some(destination) = buf.get_mut(..data.len()) else {
                let error = QHYError::BufferTooSmall {
                    needed: data.len(),
                    got: buf.len(),
                };
                tracing::error!(error = ?error);
                return Err(error);
            };
            destination.copy_from_slice(&data);

            Ok(FrameInfo {
                width,
                height,
                bits_per_pixel: bpp,
                channels,
            })
        }
    }

    /// Downloads the captured Single Frame Mode image into the caller-owned
    /// `buf`, returning its [`FrameInfo`] dimensions (blocking until the exposure
    /// completes).
    ///
    /// `buf` must be at least [`get_image_size`](Camera::get_image_size) bytes; a
    /// short buffer is rejected with [`QHYError::BufferTooSmall`] without
    /// consuming the captured image. See [`get_live_frame`](Camera::get_live_frame)
    /// for the caller-owned-buffer rationale.
    /// # Example
    ///
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera,StreamMode,ControlType};
    ///
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// camera.set_stream_mode(StreamMode::SingleFrameMode).expect("set_stream_mode failed");
    /// camera.init().expect("init failed");
    /// camera.set_parameter(ControlType::Exposure, 10000.0).expect("set_param failed"); // this is in micro seconds
    /// camera.start_single_frame_exposure().expect("start_camera_single_frame_exposure failed");
    /// let buffer_size = camera.get_image_size().expect("get_camera_image_size failed");
    /// let mut buf = vec![0u8; buffer_size];
    /// let info = camera.get_single_frame(&mut buf).expect("get_camera_single_frame failed");
    /// ```
    pub fn get_single_frame(&self, buf: &mut [u8]) -> Result<FrameInfo> {
        #[cfg(not(feature = "simulation"))]
        {
            let need = self.get_image_size()?;
            if buf.len() < need {
                let error = QHYError::BufferTooSmall {
                    needed: need,
                    got: buf.len(),
                };
                tracing::error!(error = ?error);
                return Err(error);
            }
            let mut width: u32 = 0;
            let mut height: u32 = 0;
            let mut bpp: u32 = 0;
            let mut channels: u32 = 0;
            let status = self.handle.with_handle(|handle| unsafe {
                GetQHYCCDSingleFrame(
                    handle,
                    &mut width as *mut u32,
                    &mut height as *mut u32,
                    &mut bpp as *mut u32,
                    &mut channels as *mut u32,
                    buf.as_mut_ptr(),
                )
            })?;
            match status {
                QHYCCD_SUCCESS => Ok(FrameInfo {
                    width,
                    height,
                    bits_per_pixel: bpp,
                    channels,
                }),
                _ => {
                    let error = QHYError::Sdk {
                        op: "get_single_frame",
                    };
                    tracing::error!(error = ?error);
                    Err(error)
                }
            }
        }
        #[cfg(feature = "simulation")]
        {
            let mut state = self.state.write();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }

            // Wait for exposure to complete if one is in progress (like real hardware)
            // But instead of spin-waiting, sleep for the exact remaining time
            while !state.is_exposure_complete() {
                let remaining_us = state.get_remaining_exposure_us();
                if remaining_us > 0 {
                    drop(state); // Release lock while sleeping
                    std::thread::sleep(std::time::Duration::from_micros(u64::from(remaining_us)));
                    // Reacquire lock
                    state = self.state.write();
                } else {
                    break;
                }
            }

            // Copy through the borrow rather than out of a `take`, so a short
            // buffer errors with the image still held rather than losing it.
            {
                let captured_image = state.captured_image.as_ref().ok_or(QHYError::Sdk {
                    op: "get_single_frame: no image available",
                })?;
                let Some(destination) = buf.get_mut(..captured_image.len()) else {
                    let error = QHYError::BufferTooSmall {
                        needed: captured_image.len(),
                        got: buf.len(),
                    };
                    tracing::error!(error = ?error);
                    return Err(error);
                };
                destination.copy_from_slice(captured_image);
            }

            // Delivered: drop the image and its metadata, and clear the exposure.
            state.captured_image = None;
            let metadata = state.captured_image_metadata.take().ok_or(QHYError::Sdk {
                op: "get_single_frame: no image metadata available",
            })?;
            state.exposure_start = None;

            Ok(FrameInfo {
                width: metadata.width,
                height: metadata.height,
                bits_per_pixel: metadata.bits_per_pixel,
                channels: metadata.channels,
            })
        }
    }

    /// Start a long exposure
    /// Make sure to set the exposure time before calling this function
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera,StreamMode,ControlType};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// camera.set_stream_mode(StreamMode::SingleFrameMode).expect("set_stream_mode failed");
    /// camera.init().expect("init failed");
    /// camera.set_parameter(ControlType::Exposure, 10000.0).expect("set_param failed"); // this is in micro seconds
    /// camera.start_single_frame_exposure().expect("start_single_frame_exposure failed");
    /// ```
    pub fn start_single_frame_exposure(&self) -> Result<()> {
        #[cfg(not(feature = "simulation"))]
        {
            // `ExpQHYCCDSingleFrame` has THREE documented returns, not two:
            // `QHYCCD_SUCCESS` (exposure started — wait for it) and
            // `QHYCCD_READ_DIRECTLY` (0x2001 — the frame is already captured and
            // can be read immediately) are BOTH success; only `QHYCCD_ERROR`
            // (0xFFFFFFFF) is a failure. Plain `check()` (== 0 only) would wrongly
            // reject `READ_DIRECTLY`, breaking single-frame capture on the cameras
            // / modes that take that path — INDI's indi-qhy accepts it for the same
            // reason. Reachable only against real hardware: the `simulation` arm
            // always succeeds, so no test in this crate exercises this branch.
            let status = self
                .handle
                .with_handle(|handle| unsafe { ExpQHYCCDSingleFrame(handle) })?;
            match status {
                QHYCCD_SUCCESS | QHYCCD_READ_DIRECTLY => Ok(()),
                _ => {
                    let error = QHYError::Sdk {
                        op: "start_single_frame_exposure",
                    };
                    tracing::error!(error = ?error);
                    Err(error)
                }
            }
        }
        #[cfg(feature = "simulation")]
        {
            let mut state = self.state.write();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            state.start_exposure();
            Ok(())
        }
    }

    /// Gets the remaining exposure time
    /// it needs to be called from a different thread than the one that called `start_single_frame_exposure`
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera,StreamMode,ControlType};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// camera.set_stream_mode(StreamMode::SingleFrameMode).expect("set_stream_mode failed");
    /// camera.init().expect("init failed");
    /// camera.set_parameter(ControlType::Exposure, 10000.0).expect("set_param failed"); // this is in micro seconds
    /// camera.start_single_frame_exposure().expect("start_single_frame_exposure failed");
    /// let remaining = camera.get_remaining_exposure_us().expect("get_remaining_exposure_us failed");
    /// println!("Remaining exposure time: {}", remaining);
    /// ```
    pub fn get_remaining_exposure_us(&self) -> Result<u32> {
        #[cfg(not(feature = "simulation"))]
        {
            let status = self
                .handle
                .with_handle(|handle| unsafe { GetQHYCCDExposureRemaining(handle) })?;
            match status {
                QHYCCD_ERROR => {
                    let error = QHYError::Sdk {
                        op: "get_remaining_exposure_us",
                    };
                    tracing::error!(error = ?error);
                    Err(error)
                }
                remaining if { remaining <= 100 } => Ok(0),
                remaining => Ok(remaining),
            }
        }
        #[cfg(feature = "simulation")]
        {
            let state = self.state.read();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            Ok(state.get_remaining_exposure_us())
        }
    }

    /// Stops the current exposure
    /// the image data stays in the camera and must be retrieved with `get_single_frame`
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera,StreamMode,ControlType};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// camera.set_stream_mode(StreamMode::SingleFrameMode).expect("set_stream_mode failed");
    /// camera.init().expect("init failed");
    /// camera.set_parameter(ControlType::Exposure, 10000.0).expect("set_param failed"); // this is in micro seconds
    /// camera.start_single_frame_exposure().expect("start_single_frame_exposure failed");
    /// camera.stop_exposure().expect("stop_exposure failed");
    /// ```
    pub fn stop_exposure(&self) -> Result<()> {
        #[cfg(not(feature = "simulation"))]
        {
            check(
                self.handle
                    .with_handle(|handle| unsafe { CancelQHYCCDExposing(handle) })?,
                "stop_exposure",
            )
        }
        #[cfg(feature = "simulation")]
        {
            let mut state = self.state.write();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            state.stop_exposure();
            Ok(())
        }
    }

    /// Stops the current exposure and discards the image data in the camera
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera,StreamMode,ControlType};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// camera.set_stream_mode(StreamMode::SingleFrameMode).expect("set_stream_mode failed");
    /// camera.init().expect("init failed");
    /// camera.set_parameter(ControlType::Exposure, 10000.0).expect("set_param failed"); // this is in micro seconds
    /// camera.start_single_frame_exposure().expect("start_single_frame_exposure failed");
    /// camera.abort_exposure_and_readout().expect("abort_exposure_and_readout failed");
    /// ```
    pub fn abort_exposure_and_readout(&self) -> Result<()> {
        #[cfg(not(feature = "simulation"))]
        {
            check(
                self.handle
                    .with_handle(|handle| unsafe { CancelQHYCCDExposingAndReadout(handle) })?,
                "abort_exposure_and_readout",
            )
        }
        #[cfg(feature = "simulation")]
        {
            let mut state = self.state.write();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            state.abort_exposure();
            Ok(())
        }
    }
}

// --- parameters: generic control get/set/availability + typed accessors ----
impl Camera {
    /// Returns information about the control given to the function
    /// # Returns
    /// `Err` if the control is not available
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera,ControlType};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// let control = camera.is_control_available(ControlType::Exposure).expect("is_control_available failed");
    /// println!("ControlType: {:?}", control);
    /// ```
    pub fn is_control_available(&self, control: ControlType) -> Option<u32> {
        #[cfg(not(feature = "simulation"))]
        {
            let status = self
                .handle
                .with_handle(|handle| unsafe { IsQHYCCDControlAvailable(handle, control.to_raw()) })
                .ok()?;
            match status {
                QHYCCD_ERROR => {
                    let error = QHYError::IsControlAvailable { control };
                    tracing::debug!(control = ?error);
                    None
                }
                is_supported => Some(is_supported),
            }
        }
        #[cfg(feature = "simulation")]
        {
            let state = self.state.read();
            if !state.is_open {
                return None;
            }
            // Check if control is in supported_controls
            if state.config.supported_controls.contains_key(&control) {
                // For CamColor, return the Bayer pattern value
                if control == ControlType::CamColor {
                    return state.config.bayer_pattern.map(u32::from);
                }
                // Real `IsQHYCCDControlAvailable` returns `QHYCCD_SUCCESS` (0) for an
                // available non-color control, and the real arm passes that raw
                // status through as the `Some` payload — so match it (was `Some(1)`).
                Some(0)
            } else {
                None
            }
        }
    }

    /// Returns the value for a given control
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera,ControlType};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// let exposure = camera.get_parameter(ControlType::Exposure).expect("get_parameter failed");
    /// println!("Exposure: {}", exposure);
    /// ```
    pub fn get_parameter(&self, control: ControlType) -> Result<f64> {
        #[cfg(not(feature = "simulation"))]
        {
            let res = self
                .handle
                .with_handle(|handle| unsafe { GetQHYCCDParam(handle, control.to_raw()) })?;
            if (res - QHYCCD_ERROR_F64).abs() < f64::EPSILON {
                let error = QHYError::GetParameter { control };
                tracing::error!(error = ?error);
                Err(error)
            } else {
                Ok(res)
            }
        }
        #[cfg(feature = "simulation")]
        {
            // Write lock: reading `CONTROL_CURTEMP` advances the poll-based cooling
            // ramp (`SimulatedCameraState::update_temperature`).
            let mut state = self.state.write();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            // Handle special controls
            match control {
                ControlType::CfwPort => {
                    // Position carried as its hex-ASCII code (see `cfw_slot_to_ascii`);
                    // reading advances the simulated move one poll toward the target.
                    let slot = state.poll_filter_wheel();
                    let Some(ascii) = cfw_slot_to_ascii(slot) else {
                        let error = QHYError::InvalidFilterSlot { slot };
                        tracing::error!(error = ?error);
                        return Err(error);
                    };
                    Ok(f64::from(ascii))
                }
                ControlType::CfwSlotsNum => Ok(f64::from(state.config.filter_wheel_slots)),
                ControlType::CurTemp => {
                    // Real GetQHYCCDParam(CURTEMP) tracks the CONTROL_COOLER setpoint;
                    // advance one poll-step toward it and report the new value.
                    state.update_temperature();
                    Ok(state.current_temperature)
                }
                ControlType::CurPWM => Ok(state.cooler_pwm),
                ControlType::Cooler => {
                    if state
                        .config
                        .supported_controls
                        .contains_key(&ControlType::Cooler)
                    {
                        Ok(state.target_temperature)
                    } else {
                        let error = QHYError::GetParameter { control };
                        tracing::error!(error = ?error);
                        Err(error)
                    }
                }
                ControlType::OutputDataActualBits => Ok(f64::from(state.bit_depth)),
                _ => state.parameters.get(&control).copied().ok_or_else(|| {
                    let error = QHYError::GetParameter { control };
                    tracing::error!(error = ?error);
                    error
                }),
            }
        }
    }

    /// Returns the min, max and step value for a given control
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera,ControlType};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// let (min_exposure, max_exposure, exposure_resolution) = camera.get_parameter_min_max_step(ControlType::Exposure).expect("getting min,max,step failed");
    /// ```
    pub fn get_parameter_min_max_step(&self, control: ControlType) -> Result<(f64, f64, f64)> {
        #[cfg(not(feature = "simulation"))]
        {
            let mut min: f64 = 0.0;
            let mut max: f64 = 0.0;
            let mut step: f64 = 0.0;
            let status = self.handle.with_handle(|handle| unsafe {
                GetQHYCCDParamMinMaxStep(
                    handle,
                    control.to_raw(),
                    &mut min as *mut f64,
                    &mut max as *mut f64,
                    &mut step as *mut f64,
                )
            })?;
            match status {
                QHYCCD_SUCCESS => Ok((min, max, step)),
                _ => {
                    let error = QHYError::GetMinMaxStep { control };
                    tracing::error!(error = ?error);
                    Err(error)
                }
            }
        }
        #[cfg(feature = "simulation")]
        {
            let state = self.state.read();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            state
                .config
                .supported_controls
                .get(&control)
                .copied()
                .ok_or_else(|| {
                    let error = QHYError::GetMinMaxStep { control };
                    tracing::error!(error = ?error);
                    error
                })
        }
    }

    /// Sets the value for a given control
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera,ControlType};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// camera.set_parameter(ControlType::Exposure, 2000000.0).expect("set_parameter failed");
    /// ```
    pub fn set_parameter(&self, control: ControlType, value: f64) -> Result<()> {
        #[cfg(not(feature = "simulation"))]
        {
            check(
                self.handle.with_handle(|handle| unsafe {
                    SetQHYCCDParam(handle, control.to_raw(), value)
                })?,
                "set_parameter",
            )
        }
        #[cfg(feature = "simulation")]
        {
            let mut state = self.state.write();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            // Handle special controls
            match control {
                ControlType::CfwPort => {
                    // Value is the hex-ASCII position code (see `cfw_ascii_to_slot`).
                    // Command a move; the wheel reaches the slot after a few polls
                    // (see `poll_filter_wheel`), not instantly. The decode carries a
                    // legacy fallback with no upper bound, so the command is the
                    // place that rejects a slot the wheel could not report back.
                    state.command_filter_wheel(cfw_ascii_to_slot(quantize::to_u32(value)))?;
                }
                ControlType::Cooler => {
                    state.target_temperature = value;
                }
                ControlType::ManualPWM => {
                    state.cooler_pwm = value;
                }
                ControlType::Exposure => {
                    state.exposure_duration_us = quantize::to_u64(value);
                    state.parameters.insert(control, value);
                }
                _ => {
                    state.parameters.insert(control, value);
                }
            }
            Ok(())
        }
    }

    /// Convinience function that sets the value for a given control if it is available
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera,ControlType};
    ///
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// camera.set_if_available(ControlType::TransferBit, 16.0).expect("failed to set usb transfer mode");
    /// ```
    pub fn set_if_available(&self, control: ControlType, value: f64) -> Result<()> {
        match self.is_control_available(control) {
            Some(_) => self.set_parameter(control, value),
            None => Err(QHYError::IsControlAvailable { control }),
        }
    }

    /// Returns `true` if a filter wheel is plugged into the given camera
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera,ControlType};
    ///
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// let is_cfw_plugged_in = camera.is_cfw_plugged_in().expect("is_cfw_plugged_in failed");
    /// println!("Is filter wheel plugged in: {}", is_cfw_plugged_in);
    /// ```
    pub fn is_cfw_plugged_in(&self) -> Result<bool> {
        #[cfg(not(feature = "simulation"))]
        {
            let status = self
                .handle
                .with_handle(|handle| unsafe { IsQHYCCDCFWPlugged(handle) })?;
            match status {
                QHYCCD_SUCCESS => Ok(true),
                QHYCCD_ERROR => Ok(false),
                _ => {
                    let error = QHYError::Sdk {
                        op: "is_cfw_plugged_in",
                    };
                    tracing::error!(error = ?error);
                    Err(error)
                }
            }
        }
        #[cfg(feature = "simulation")]
        {
            let state = self.state.read();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            Ok(state.config.filter_wheel_slots > 0)
        }
    }

    // --- typed accessors ------------------------------------------------------
    //
    // Thin, unit-labelled wrappers over the generic `get_parameter` /
    // `set_parameter` / `get_parameter_min_max_step` surface, mirroring
    // `svbony camera.rs`'s `gain()` / `exposure_us()` / … accessors. They are
    // the routine way to read/write the well-known controls; the generic
    // `*_parameter(ControlType, )` methods remain for `Other` and any control
    // without a dedicated accessor. QHY temperatures are already whole °C, so —
    // unlike svbony's 0.1 °C `SVB_*_TEMPERATURE` — no decode is applied.
    // Backend-agnostic: each delegates to a forked method above.

    /// Current sensor gain (`ControlType::Gain`).
    pub fn gain(&self) -> Result<f64> {
        self.get_parameter(ControlType::Gain)
    }

    /// Set the sensor gain (`ControlType::Gain`).
    pub fn set_gain(&self, gain: f64) -> Result<()> {
        self.set_parameter(ControlType::Gain, gain)
    }

    /// `(min, max, step)` for gain (`ControlType::Gain`).
    pub fn gain_range(&self) -> Result<(f64, f64, f64)> {
        self.get_parameter_min_max_step(ControlType::Gain)
    }

    /// Current sensor offset / black level (`ControlType::Offset`).
    pub fn offset(&self) -> Result<f64> {
        self.get_parameter(ControlType::Offset)
    }

    /// Set the sensor offset / black level (`ControlType::Offset`).
    pub fn set_offset(&self, offset: f64) -> Result<()> {
        self.set_parameter(ControlType::Offset, offset)
    }

    /// `(min, max, step)` for offset (`ControlType::Offset`).
    pub fn offset_range(&self) -> Result<(f64, f64, f64)> {
        self.get_parameter_min_max_step(ControlType::Offset)
    }

    /// Current exposure time in microseconds (`ControlType::Exposure`).
    pub fn exposure_us(&self) -> Result<f64> {
        self.get_parameter(ControlType::Exposure)
    }

    /// Set the exposure time in microseconds (`ControlType::Exposure`).
    pub fn set_exposure_us(&self, exposure_us: f64) -> Result<()> {
        self.set_parameter(ControlType::Exposure, exposure_us)
    }

    /// `(min, max, step)` for exposure in microseconds (`ControlType::Exposure`).
    pub fn exposure_range_us(&self) -> Result<(f64, f64, f64)> {
        self.get_parameter_min_max_step(ControlType::Exposure)
    }

    /// Current sensor temperature in °C (`ControlType::CurTemp`). Reported
    /// independently of whether cooling is on.
    pub fn current_temperature_celsius(&self) -> Result<f64> {
        self.get_parameter(ControlType::CurTemp)
    }

    /// Engage the cooler's auto-regulation to `celsius` (`ControlType::Cooler`).
    ///
    /// Actuates hardware — callers must respect the workspace's *no actuation on
    /// connect* tenet (never call from a connect/reconnect path; see
    /// `docs/workspace.md`).
    pub fn set_target_temperature_celsius(&self, celsius: f64) -> Result<()> {
        self.set_parameter(ControlType::Cooler, celsius)
    }

    /// Set a fixed manual cooler duty cycle, 0–255 (`ControlType::ManualPWM`);
    /// writing `0.0` stops the cooler.
    ///
    /// Actuates hardware — see [`Camera::set_target_temperature_celsius`]'s
    /// tenet note.
    pub fn set_manual_cooler_pwm(&self, pwm: f64) -> Result<()> {
        self.set_parameter(ControlType::ManualPWM, pwm)
    }

    /// Current cooler power as the raw SDK PWM, 0–255 (`ControlType::CurPWM`).
    pub fn cooler_power_raw(&self) -> Result<f64> {
        self.get_parameter(ControlType::CurPWM)
    }

    /// Number of filter-wheel slots (`ControlType::CfwSlotsNum`).
    pub fn cfw_slot_count(&self) -> Result<u32> {
        Ok(quantize::to_u32(
            self.get_parameter(ControlType::CfwSlotsNum)?,
        ))
    }

    /// Current 0-indexed filter-wheel position, decoding the SDK's hex-ASCII
    /// `ControlType::CfwPort` value (`'0'` == slot 0 .. `'F'` == slot 15).
    pub fn cfw_position(&self) -> Result<u32> {
        Ok(cfw_ascii_to_slot(quantize::to_u32(
            self.get_parameter(ControlType::CfwPort)?,
        )))
    }

    /// Command the filter wheel to a 0-indexed slot, encoding it as the SDK's
    /// hex-ASCII `ControlType::CfwPort` code (`'0'` == slot 0 .. `'F'` == slot 15).
    /// A slot past the sixteenth has no such code, and is rejected with
    /// [`QHYError::InvalidFilterSlot`] rather than commanded as some other slot.
    ///
    /// Actuates hardware — see [`Camera::set_target_temperature_celsius`]'s
    /// tenet note.
    pub fn set_cfw_position(&self, position: u32) -> Result<()> {
        let Some(ascii) = cfw_slot_to_ascii(position) else {
            let error = QHYError::InvalidFilterSlot { slot: position };
            tracing::error!(error = ?error);
            return Err(error);
        };
        self.set_parameter(ControlType::CfwPort, f64::from(ascii))
    }
}

// --- readout modes: count / name / resolution / current --------------------
impl Camera {
    /// Returns the number of readout modes of the camera
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// let number_of_readout_modes = camera.get_number_of_readout_modes().expect("get_number_of_readout_modes failed");
    /// println!("Number of readout modes: {}", number_of_readout_modes);
    /// ```
    pub fn get_number_of_readout_modes(&self) -> Result<u32> {
        #[cfg(not(feature = "simulation"))]
        {
            let mut num: u32 = 0;
            let status = self.handle.with_handle(|handle| unsafe {
                GetQHYCCDNumberOfReadModes(handle, &mut num as *mut u32)
            })?;
            match status {
                // Match on success, not on the `QHYCCD_ERROR` sentinel, so any
                // non-success return is rejected instead of yielding the
                // zero-initialised `num` as a valid "0 readout modes" (aligns with
                // the sibling readout getters + INDI).
                QHYCCD_SUCCESS => Ok(num),
                _ => {
                    let error = QHYError::Sdk {
                        op: "get_number_of_readout_modes",
                    };
                    tracing::error!(error = ?error);
                    Err(error)
                }
            }
        }
        #[cfg(feature = "simulation")]
        {
            let state = self.state.read();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            // The real path reports the count in a `u32` out-parameter, so a
            // configured list too long to fit one is a config the SDK could not
            // have described — the error arm the signature already carries.
            u32::try_from(state.config.readout_modes.len()).map_err(|_| QHYError::Sdk {
                op: "get_number_of_readout_modes",
            })
        }
    }

    /// Returns the readout mode name with the given index. Make sure to check the number of readout modes.
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// let readout_mode_name = camera.get_readout_mode_name(0).expect("get_readout_mode_name failed");
    /// println!("Readout mode name: {}", readout_mode_name);
    /// ```
    pub fn get_readout_mode_name(&self, index: u32) -> Result<String> {
        #[cfg(not(feature = "simulation"))]
        {
            let mut name: [c_char; 128] = [0; 128];
            let status = self.handle.with_handle(|handle| unsafe {
                GetQHYCCDReadModeName(handle, index, name.as_mut_ptr())
            })?;
            match status {
                QHYCCD_SUCCESS => {
                    // Bounded decode (see `get_model`): scan for the NUL within the
                    // fixed 128-byte buffer rather than an unbounded `CStr::from_ptr`.
                    let bytes = unsafe {
                        std::slice::from_raw_parts(name.as_ptr().cast::<u8>(), name.len())
                    };
                    let name = CStr::from_bytes_until_nul(bytes)
                        .map_err(|_| QHYError::Sdk {
                            op: "get_readout_mode_name",
                        })?
                        .to_str()
                        .inspect_err(|error| tracing::error!(error = ?error))?;
                    Ok(name.to_string())
                }
                // Match on success, not on the `QHYCCD_ERROR` sentinel: reject any
                // non-success return rather than pass a possibly-untouched buffer
                // through (aligns with the sibling readout getters + INDI).
                _ => {
                    let error = QHYError::Sdk {
                        op: "get_readout_mode_name",
                    };
                    tracing::error!(error = ?error);
                    Err(error)
                }
            }
        }
        #[cfg(feature = "simulation")]
        {
            let state = self.state.read();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            usize::try_from(index)
                .ok()
                .and_then(|i| state.config.readout_modes.get(i))
                .map(|(name, _)| name.clone())
                .ok_or(QHYError::Sdk {
                    op: "get_readout_mode_name",
                })
        }
    }

    /// Returns the readout mode resolution with the given index. Make sure to check the number of readout modes.
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// let (width, height) = camera.get_readout_mode_resolution(0).expect("get_readout_mode_resolution failed");
    /// println!("Readout mode resolution: {}x{}", width, height);
    /// ```
    pub fn get_readout_mode_resolution(&self, index: u32) -> Result<(u32, u32)> {
        #[cfg(not(feature = "simulation"))]
        {
            let mut width: u32 = 0;
            let mut height: u32 = 0;
            let status = self.handle.with_handle(|handle| unsafe {
                GetQHYCCDReadModeResolution(
                    handle,
                    index,
                    &mut width as *mut u32,
                    &mut height as *mut u32,
                )
            })?;
            match status {
                QHYCCD_SUCCESS => Ok((width, height)),
                _ => {
                    let error = QHYError::Sdk {
                        op: "get_readout_mode_resolution",
                    };
                    tracing::error!(error = ?error);
                    Err(error)
                }
            }
        }
        #[cfg(feature = "simulation")]
        {
            let state = self.state.read();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            usize::try_from(index)
                .ok()
                .and_then(|i| state.config.readout_modes.get(i))
                .map(|(_, res)| *res)
                .ok_or(QHYError::Sdk {
                    op: "get_readout_mode_resolution",
                })
        }
    }

    /// Returns the current readout mode
    /// # Example
    /// ```no_run
    /// use qhyccd_rs::{Sdk,Camera};
    /// let sdk = Sdk::new().expect("SDK::new failed");
    /// let camera = sdk.cameras().last().expect("no camera found");
    /// camera.open().expect("open failed");
    /// let readout_mode = camera.get_readout_mode().expect("get_readout_mode failed");
    /// println!("Readout mode: {}", readout_mode);
    /// ```
    pub fn get_readout_mode(&self) -> Result<u32> {
        #[cfg(not(feature = "simulation"))]
        {
            let mut mode: u32 = 0;
            let status = self.handle.with_handle(|handle| unsafe {
                GetQHYCCDReadMode(handle, &mut mode as *mut u32)
            })?;
            match status {
                QHYCCD_SUCCESS => Ok(mode),
                _ => {
                    let error = QHYError::Sdk {
                        op: "get_readout_mode",
                    };
                    tracing::error!(error = ?error);
                    Err(error)
                }
            }
        }
        #[cfg(feature = "simulation")]
        {
            let state = self.state.read();
            if !state.is_open {
                return Err(QHYError::CameraNotOpen);
            }
            Ok(state.readout_mode)
        }
    }
}

/// The ownership rule the real arm rests on: an SDK call and the `CloseQHYCCD`
/// that would free the handle under it cannot overlap, while two SDK calls can.
///
/// These are the only tests in the crate that reach the real backend's types.
/// They build because [`HandleCell`] is gated `any(not(simulation), test)`
/// rather than on the feature alone, and they need no SDK: nothing here
/// dereferences the handle, and the FFI `Drop` is compiled out in the
/// simulated configuration these tests are built in.
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod handle_cell_tests {
    use super::{HandleCell, QHYCCDHandle};
    use crate::QHYError;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    /// How long a test waits on another thread before calling it wedged. Long
    /// enough that a loaded runner cannot fail it, short enough to report
    /// rather than hang out a test timeout.
    const RENDEZVOUS: Duration = Duration::from_secs(30);

    /// A cell holding a handle no SDK call will ever see: the pointer is null,
    /// `with_handle` only hands it to the closure, and the FFI `Drop` that
    /// would pass it to `CloseQHYCCD` is compiled out here.
    fn open_cell() -> HandleCell {
        let cell = HandleCell::new();
        *cell.write() = Some(QHYCCDHandle {
            ptr: std::ptr::null(),
        });
        cell
    }

    #[test]
    fn a_call_holds_the_cell_a_close_needs() {
        let cell = open_cell();
        cell.with_handle(|_| {
            assert!(
                cell.try_write().is_none(),
                "a close could take the cell while a call was inside the SDK: \
                 `CloseQHYCCD` would free the handle that call is using"
            );
        })
        .unwrap();
        assert!(
            cell.try_write().is_some(),
            "the cell stayed locked after the call returned, so a close could never run"
        );
    }

    #[test]
    fn calls_do_not_wait_for_each_other() {
        let cell = Arc::new(open_cell());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let held = Arc::clone(&cell);
        let call = thread::spawn(move || {
            held.with_handle(|_| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(RENDEZVOUS).unwrap();
            })
            .unwrap();
        });

        entered_rx.recv_timeout(RENDEZVOUS).unwrap();
        // Non-blocking on purpose: an exclusive lock would make this test hang
        // out its timeout rather than say what went wrong.
        assert!(
            cell.try_read().is_some(),
            "a second SDK call was excluded while the first was in flight — a \
             temperature poll would block behind a readout"
        );

        release_tx.send(()).unwrap();
        call.join().unwrap();
    }

    #[test]
    fn an_unopened_cell_refuses_the_call() {
        let cell = HandleCell::new();
        let mut ran = false;
        let err = cell
            .with_handle(|_| {
                ran = true;
            })
            .unwrap_err();
        assert_eq!(err, QHYError::CameraNotOpen);
        assert!(!ran, "the closure reached the SDK with no open handle");
    }
}

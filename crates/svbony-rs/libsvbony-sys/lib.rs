//! Hand-written FFI bindings for `SVBony`'s `SVBCameraSDK` (SDK **1.13.4**,
//! ground truth verified 2026-07-21 directly against
//! `SVBCameraSDK.h` as vendored by indi-3rdparty's `libsvbony`).
//!
//! ## Why hand-written, not `bindgen` (unlike `libzwo-sys`)
//!
//! `libzwo-sys` vendors ZWO's actual MIT-licensed SDK header text and runs
//! `bindgen` over it. `SVBony`'s SDK header carries **no license text
//! anywhere** — not in the header itself, not in the INDI packaging, not in
//! any accompanying file — so there is no written redistribution grant for
//! the header text. This crate therefore does **not** vendor
//! `SVBCameraSDK.h`; the `extern "C"` declarations, struct layouts, and
//! named constants below are hand-transcribed from reading that header (the
//! facts — function names, parameter order/types, struct field order, enum
//! ordinal values — are not copyrightable), the same posture `libqhyccd-sys`
//! takes toward QHY's similarly unlicensed header. See
//! `docs/plans/archive/svbony-camera.md` ("Verified SDK ground truth") for the full
//! provenance trail and citation of the source URL.
//!
//! ## Enum representation
//!
//! The SDK's C enums have no explicit values except where noted below, so
//! ordinal position IS the value. Rather than model them as `#[repr(<int>)]`
//! Rust `enum`s (whose exact underlying-type ABI can vary across C
//! compilers/platforms), each is a plain `i32` type alias with `pub const`
//! values — mirroring `libqhyccd-sys`'s style and sidestepping enum-size ABI
//! risk entirely.
//!
//! ## Linking
//!
//! `build.rs` emits the native link directives for the system-installed
//! `libSVBCameraSDK` (+ `libusb-1.0`) unless `SVBONY_SKIP_NATIVE_LINK=1` is
//! set, in which case it also sets the `svbony_skip_link` cfg, which gates
//! off the `#[link(...)]` attribute below (see `build.rs` for why: the
//! compile-time attribute records a native-library dependency independently
//! of the build script's `cargo:rustc-link-lib` directives, so both must be
//! gated together for a link-free simulation build).

use std::os::raw::{c_char, c_int, c_long, c_uchar, c_uint};

/// Keeps `libusb-1.0` in the consumer binary's `DT_NEEDED`.
///
/// The vendored `libSVBCameraSDK.so`/`.dylib` blob references `libusb_*`
/// symbols **without** declaring libusb in its own `DT_NEEDED` (byte-
/// verified via `readelf -d`, see `install-svbony-sdk`'s header comment),
/// so the *consumer binary* must carry the libusb dependency for the loader
/// to resolve them at the first camera-SDK call. The plain `-lusb-1.0`
/// directive from `build.rs` is not sufficient on its own: the linker's
/// `--as-needed` default (GNU ld and rust-lld alike) drops a library no
/// regular object references, and `svbony-rs`'s own code never calls a
/// `libusb_*` symbol directly — only the SDK blob does, internally. This
/// `#[used]` function-pointer static is that regular-object reference,
/// mirroring `libzwo-sys`'s `zwo_keep_udev`. (A `-Wl,--no-as-needed` /
/// `-Wl,--as-needed` bracket around the link-lib line, tried first, does
/// NOT work here: `cargo:rustc-link-arg` only takes effect for the build
/// script's *own* package's bin/cdylib/test/example targets, not
/// transitively for downstream binaries like `svbony-camera` — verified
/// empirically, see issue #681.) The `svbony_keep_libusb` cfg is emitted by
/// `build.rs` exactly when the libusb link directive is (macOS/Linux, not
/// `SVBONY_SKIP_NATIVE_LINK`).
#[cfg(svbony_keep_libusb)]
mod libusb_keepalive {
    extern "C" {
        fn libusb_init(ctx: *mut std::os::raw::c_void) -> std::os::raw::c_int;
    }
    #[used]
    #[no_mangle]
    static SVBONY_SYS_KEEP_LIBUSB: unsafe extern "C" fn(
        *mut std::os::raw::c_void,
    ) -> std::os::raw::c_int = libusb_init;
}

// ---- SVB_BAYER_PATTERN ------------------------------------------------------

pub type SvbBayerPattern = i32;
pub const SVB_BAYER_RG: SvbBayerPattern = 0;
pub const SVB_BAYER_BG: SvbBayerPattern = 1;
pub const SVB_BAYER_GR: SvbBayerPattern = 2;
pub const SVB_BAYER_GB: SvbBayerPattern = 3;

// ---- SVB_IMG_TYPE ------------------------------------------------------------

pub type SvbImgType = i32;
pub const SVB_IMG_RAW8: SvbImgType = 0;
pub const SVB_IMG_RAW10: SvbImgType = 1;
pub const SVB_IMG_RAW12: SvbImgType = 2;
pub const SVB_IMG_RAW14: SvbImgType = 3;
pub const SVB_IMG_RAW16: SvbImgType = 4;
pub const SVB_IMG_Y8: SvbImgType = 5;
pub const SVB_IMG_Y10: SvbImgType = 6;
pub const SVB_IMG_Y12: SvbImgType = 7;
pub const SVB_IMG_Y14: SvbImgType = 8;
pub const SVB_IMG_Y16: SvbImgType = 9;
pub const SVB_IMG_RGB24: SvbImgType = 10;
pub const SVB_IMG_RGB32: SvbImgType = 11;
/// Sentinel array terminator (e.g. in `SVB_CAMERA_PROPERTY::SupportedVideoFormat`).
pub const SVB_IMG_END: SvbImgType = -1;

// ---- SVB_GUIDE_DIRECTION ------------------------------------------------------

pub type SvbGuideDirection = i32;
pub const SVB_GUIDE_NORTH: SvbGuideDirection = 0;
pub const SVB_GUIDE_SOUTH: SvbGuideDirection = 1;
pub const SVB_GUIDE_EAST: SvbGuideDirection = 2;
pub const SVB_GUIDE_WEST: SvbGuideDirection = 3;

// ---- SVB_FLIP_STATUS ------------------------------------------------------

pub type SvbFlipStatus = i32;
pub const SVB_FLIP_NONE: SvbFlipStatus = 0;
pub const SVB_FLIP_HORIZ: SvbFlipStatus = 1;
pub const SVB_FLIP_VERT: SvbFlipStatus = 2;
pub const SVB_FLIP_BOTH: SvbFlipStatus = 3;

// ---- SVB_CAMERA_MODE ------------------------------------------------------

pub type SvbCameraMode = i32;
pub const SVB_MODE_NORMAL: SvbCameraMode = 0;
pub const SVB_MODE_TRIG_SOFT: SvbCameraMode = 1;
pub const SVB_MODE_TRIG_RISE_EDGE: SvbCameraMode = 2;
pub const SVB_MODE_TRIG_FALL_EDGE: SvbCameraMode = 3;
pub const SVB_MODE_TRIG_DOUBLE_EDGE: SvbCameraMode = 4;
pub const SVB_MODE_TRIG_HIGH_LEVEL: SvbCameraMode = 5;
pub const SVB_MODE_TRIG_LOW_LEVEL: SvbCameraMode = 6;
/// Sentinel array terminator (in `SVB_SUPPORTED_MODE::SupportedCameraMode`).
pub const SVB_MODE_END: SvbCameraMode = -1;

// ---- SVB_TRIG_OUTPUT (tag) / SVB_TRIG_OUTPUT_PIN (typedef alias) -------------
//
// NOTE the tag/typedef-alias mismatch in the real header: the enum TAG is
// `SVB_TRIG_OUTPUT` but the TYPEDEF alias actually used in function
// signatures (`SVBSetTriggerOutputIOConf` etc.) is `SVB_TRIG_OUTPUT_PIN`. We
// name the Rust alias after the typedef, since that's what call sites use.

pub type SvbTrigOutputPin = i32;
pub const SVB_TRIG_OUTPUT_PINA: SvbTrigOutputPin = 0;
pub const SVB_TRIG_OUTPUT_PINB: SvbTrigOutputPin = 1;
pub const SVB_TRIG_OUTPUT_NONE: SvbTrigOutputPin = -1;

// ---- SVB_ERROR_CODE ------------------------------------------------------

pub type SvbErrorCode = i32;
pub const SVB_SUCCESS: SvbErrorCode = 0;
pub const SVB_ERROR_INVALID_INDEX: SvbErrorCode = 1;
pub const SVB_ERROR_INVALID_ID: SvbErrorCode = 2;
pub const SVB_ERROR_INVALID_CONTROL_TYPE: SvbErrorCode = 3;
pub const SVB_ERROR_CAMERA_CLOSED: SvbErrorCode = 4;
pub const SVB_ERROR_CAMERA_REMOVED: SvbErrorCode = 5;
pub const SVB_ERROR_INVALID_PATH: SvbErrorCode = 6;
pub const SVB_ERROR_INVALID_FILEFORMAT: SvbErrorCode = 7;
pub const SVB_ERROR_INVALID_SIZE: SvbErrorCode = 8;
pub const SVB_ERROR_INVALID_IMGTYPE: SvbErrorCode = 9;
pub const SVB_ERROR_OUTOF_BOUNDARY: SvbErrorCode = 10;
pub const SVB_ERROR_TIMEOUT: SvbErrorCode = 11;
pub const SVB_ERROR_INVALID_SEQUENCE: SvbErrorCode = 12;
pub const SVB_ERROR_BUFFER_TOO_SMALL: SvbErrorCode = 13;
pub const SVB_ERROR_VIDEO_MODE_ACTIVE: SvbErrorCode = 14;
pub const SVB_ERROR_EXPOSURE_IN_PROGRESS: SvbErrorCode = 15;
pub const SVB_ERROR_GENERAL_ERROR: SvbErrorCode = 16;
pub const SVB_ERROR_INVALID_MODE: SvbErrorCode = 17;
pub const SVB_ERROR_INVALID_DIRECTION: SvbErrorCode = 18;
pub const SVB_ERROR_UNKNOW_SENSOR_TYPE: SvbErrorCode = 19;
pub const SVB_ERROR_END: SvbErrorCode = 20;

// ---- SVB_BOOL ------------------------------------------------------

pub type SvbBool = i32;
pub const SVB_FALSE: SvbBool = 0;
pub const SVB_TRUE: SvbBool = 1;

// ---- SVB_CONTROL_TYPE ------------------------------------------------------

pub type SvbControlType = i32;
pub const SVB_GAIN: SvbControlType = 0;
pub const SVB_EXPOSURE: SvbControlType = 1;
pub const SVB_GAMMA: SvbControlType = 2;
pub const SVB_GAMMA_CONTRAST: SvbControlType = 3;
pub const SVB_WB_R: SvbControlType = 4;
pub const SVB_WB_G: SvbControlType = 5;
pub const SVB_WB_B: SvbControlType = 6;
pub const SVB_FLIP: SvbControlType = 7;
pub const SVB_FRAME_SPEED_MODE: SvbControlType = 8;
pub const SVB_CONTRAST: SvbControlType = 9;
pub const SVB_SHARPNESS: SvbControlType = 10;
pub const SVB_SATURATION: SvbControlType = 11;
pub const SVB_AUTO_TARGET_BRIGHTNESS: SvbControlType = 12;
/// The ASCOM *Offset*-equivalent control.
///
/// Two dead macros in the real header
/// (`SVB_BRIGHTNESS` / `SVB_AUTO_MAX_BRIGHTNESS`) alias `SVB_OFFSET` and
/// `SVB_AUTO_TARGET_BRIGHTNESS` respectively; `SVB_OFFSET` is never actually
/// defined anywhere in the header (apparently stale dead code), so neither
/// macro is ported here — use `SVB_BLACK_LEVEL` directly.
pub const SVB_BLACK_LEVEL: SvbControlType = 13;
pub const SVB_COOLER_ENABLE: SvbControlType = 14;
/// Unit is 0.1 °C.
///
/// `SVBGetControlValue`'s own docs separately describe the
/// target temperature as "an integer" in one place, which is ambiguous
/// against the 0.1 °C unit note elsewhere in the same header — flagged here
/// rather than silently resolved; needs real-hardware confirmation.
pub const SVB_TARGET_TEMPERATURE: SvbControlType = 15;
/// Unit is 0.1 °C.
pub const SVB_CURRENT_TEMPERATURE: SvbControlType = 16;
pub const SVB_COOLER_POWER: SvbControlType = 17;
pub const SVB_BAD_PIXEL_CORRECTION_ENABLE: SvbControlType = 18;
pub const SVB_BAD_PIXEL_CORRECTION_THRESHOLD: SvbControlType = 19;

// ---- SVB_EXPOSURE_STATUS ------------------------------------------------------
//
// NOTE: no function in this SDK returns this status directly — there is no
// `SVBGetExposureStatus`. Exposure state is entirely video-capture-driven
// (`SVBStartVideoCapture` / `SVBSendSoftTrigger` / `SVBGetVideoData`). Kept
// here only for documentation completeness / forward-compat should a future
// SDK version add a getter.

pub type SvbExposureStatus = i32;
pub const SVB_EXP_IDLE: SvbExposureStatus = 0;
pub const SVB_EXP_WORKING: SvbExposureStatus = 1;
pub const SVB_EXP_SUCCESS: SvbExposureStatus = 2;
pub const SVB_EXP_FAILED: SvbExposureStatus = 3;

// ---- structs (#[repr(C)]) ------------------------------------------------------

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SvbCameraInfo {
    pub friendly_name: [c_char; 32],
    pub camera_sn: [c_char; 32],
    pub port_type: [c_char; 32],
    pub device_id: c_uint,
    pub camera_id: c_int,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SvbCameraProperty {
    pub max_height: c_long,
    pub max_width: c_long,
    pub is_color_cam: SvbBool,
    pub bayer_pattern: SvbBayerPattern,
    pub supported_bins: [c_int; 16],
    pub supported_video_format: [SvbImgType; 8],
    pub max_bit_depth: c_int,
    pub is_trigger_cam: SvbBool,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SvbCameraPropertyEx {
    pub support_pulse_guide: SvbBool,
    pub support_control_temp: SvbBool,
    pub unused: [c_int; 64],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SvbControlCaps {
    pub name: [c_char; 64],
    pub description: [c_char; 128],
    pub max_value: c_long,
    pub min_value: c_long,
    pub default_value: c_long,
    pub is_auto_supported: SvbBool,
    pub is_writable: SvbBool,
    pub control_type: SvbControlType,
    pub unused: [c_char; 32],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SvbId {
    pub id: [c_uchar; 64],
}

/// `SVB_SN` — a typedef alias of `SVB_ID` in the real header.
pub type SvbSn = SvbId;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SvbSupportedMode {
    pub supported_camera_mode: [SvbCameraMode; 16],
}

// ---- functions ------------------------------------------------------

// `qhyccd_skip_link`-style escape hatch (see the crate docs above and
// `build.rs`): when `SVBONY_SKIP_NATIVE_LINK` is set, `build.rs` sets the
// `svbony_skip_link` cfg, which gates off this compile-time `#[link]`
// attribute — otherwise it would force the native-library search
// independently of the build script's (also-omitted) link directives.
#[cfg_attr(not(svbony_skip_link), link(name = "SVBCameraSDK", kind = "dylib"))]
extern "C" {
    pub fn SVBGetNumOfConnectedCameras() -> c_int;

    pub fn SVBGetCameraInfo(info: *mut SvbCameraInfo, camera_index: c_int) -> SvbErrorCode;

    pub fn SVBGetCameraProperty(camera_id: c_int, prop: *mut SvbCameraProperty) -> SvbErrorCode;

    pub fn SVBGetCameraPropertyEx(
        camera_id: c_int,
        prop_ex: *mut SvbCameraPropertyEx,
    ) -> SvbErrorCode;

    pub fn SVBOpenCamera(camera_id: c_int) -> SvbErrorCode;

    pub fn SVBCloseCamera(camera_id: c_int) -> SvbErrorCode;

    pub fn SVBGetNumOfControls(camera_id: c_int, num_controls: *mut c_int) -> SvbErrorCode;

    /// `control_index` is an INDEX into the camera's control list, NOT a
    /// [`SvbControlType`] value.
    pub fn SVBGetControlCaps(
        camera_id: c_int,
        control_index: c_int,
        caps: *mut SvbControlCaps,
    ) -> SvbErrorCode;

    pub fn SVBGetControlValue(
        camera_id: c_int,
        control_type: SvbControlType,
        value: *mut c_long,
        is_auto: *mut SvbBool,
    ) -> SvbErrorCode;

    pub fn SVBSetControlValue(
        camera_id: c_int,
        control_type: SvbControlType,
        value: c_long,
        is_auto: SvbBool,
    ) -> SvbErrorCode;

    pub fn SVBGetOutputImageType(camera_id: c_int, image_type: *mut SvbImgType) -> SvbErrorCode;

    pub fn SVBSetOutputImageType(camera_id: c_int, image_type: SvbImgType) -> SvbErrorCode;

    /// `width % 8 == 0` and `height % 2 == 0` are SDK requirements.
    pub fn SVBSetROIFormat(
        camera_id: c_int,
        start_x: c_int,
        start_y: c_int,
        width: c_int,
        height: c_int,
        bin: c_int,
    ) -> SvbErrorCode;

    pub fn SVBSetROIFormatEx(
        camera_id: c_int,
        start_x: c_int,
        start_y: c_int,
        width: c_int,
        height: c_int,
        bin: c_int,
        mode: c_int,
    ) -> SvbErrorCode;

    pub fn SVBGetROIFormat(
        camera_id: c_int,
        start_x: *mut c_int,
        start_y: *mut c_int,
        width: *mut c_int,
        height: *mut c_int,
        bin: *mut c_int,
    ) -> SvbErrorCode;

    pub fn SVBGetROIFormatEx(
        camera_id: c_int,
        start_x: *mut c_int,
        start_y: *mut c_int,
        width: *mut c_int,
        height: *mut c_int,
        bin: *mut c_int,
        mode: *mut c_int,
    ) -> SvbErrorCode;

    pub fn SVBGetDroppedFrames(camera_id: c_int, drop_frames: *mut c_int) -> SvbErrorCode;

    /// Returns [`SVB_ERROR_EXPOSURE_IN_PROGRESS`] if "snap mode" is active.
    pub fn SVBStartVideoCapture(camera_id: c_int) -> SvbErrorCode;

    pub fn SVBStopVideoCapture(camera_id: c_int) -> SvbErrorCode;

    /// Buffer size in bytes: 8-bit mono = `w*h`; 16-bit mono = `w*h*2`;
    /// RGB24 = `w*h*3`. `wait_ms`: `-1` waits forever; the SDK's own
    /// recommendation is `exposure*2 + 500ms`.
    pub fn SVBGetVideoData(
        camera_id: c_int,
        buffer: *mut c_uchar,
        buffer_size: c_long,
        wait_ms: c_int,
    ) -> SvbErrorCode;

    pub fn SVBWhiteBalanceOnce(camera_id: c_int) -> SvbErrorCode;

    /// `firmware_version` buffer must be at least 64 bytes.
    pub fn SVBGetCameraFirmwareVersion(
        camera_id: c_int,
        firmware_version: *mut c_char,
    ) -> SvbErrorCode;

    /// Global, no camera id, no error code — e.g. `"1, 13, 0503"`.
    pub fn SVBGetSDKVersion() -> *const c_char;

    /// Only meaningful if the camera's `IsTriggerCam` property is true.
    pub fn SVBGetCameraSupportMode(
        camera_id: c_int,
        supported_mode: *mut SvbSupportedMode,
    ) -> SvbErrorCode;

    pub fn SVBGetCameraMode(camera_id: c_int, mode: *mut SvbCameraMode) -> SvbErrorCode;

    pub fn SVBSetCameraMode(camera_id: c_int, mode: SvbCameraMode) -> SvbErrorCode;

    pub fn SVBSendSoftTrigger(camera_id: c_int) -> SvbErrorCode;

    pub fn SVBGetSerialNumber(camera_id: c_int, serial_number: *mut SvbSn) -> SvbErrorCode;

    pub fn SVBSetTriggerOutputIOConf(
        camera_id: c_int,
        pin: SvbTrigOutputPin,
        pin_high: SvbBool,
        delay: c_long,
        duration: c_long,
    ) -> SvbErrorCode;

    pub fn SVBGetTriggerOutputIOConf(
        camera_id: c_int,
        pin: SvbTrigOutputPin,
        pin_high: *mut SvbBool,
        delay: *mut c_long,
        duration: *mut c_long,
    ) -> SvbErrorCode;

    /// `duration` is in milliseconds.
    pub fn SVBPulseGuide(
        camera_id: c_int,
        direction: SvbGuideDirection,
        duration: c_int,
    ) -> SvbErrorCode;

    /// Pixel size in microns.
    pub fn SVBGetSensorPixelSize(camera_id: c_int, pixel_size: *mut f32) -> SvbErrorCode;

    pub fn SVBCanPulseGuide(camera_id: c_int, can_pulse_guide: *mut SvbBool) -> SvbErrorCode;

    pub fn SVBSetAutoSaveParam(camera_id: c_int, enable: SvbBool) -> SvbErrorCode;

    /// `min_version` buffer must be at least 64 bytes.
    pub fn SVBIsCameraNeedToUpgrade(
        camera_id: c_int,
        need_to_upgrade: *mut SvbBool,
        min_version: *mut c_char,
    ) -> SvbErrorCode;

    pub fn SVBRestoreDefaultParam(camera_id: c_int) -> SvbErrorCode;
}

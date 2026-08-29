# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `Camera::restore_default_param` (`SVBRestoreDefaultParam`) and
  `Camera::set_auto_save_param` (`SVBSetAutoSaveParam`) safe wrappers. The
  SDK's auto-save (on by default) persists the whole camera parameter block
  to `<model>_Cfg_SAVE.bin` in the process's working directory at close and
  reloads it at the next open; drivers wanting deterministic connect state
  restore defaults and turn auto-save off right after opening, as
  `indi_svbony_ccd` does.
- `Camera::get_video_data_calls`, a simulation-only read-only count of the
  `get_video_data` calls a camera has served, frame or timeout. It makes a
  driver's retrieval loop observable from a test: the loop's first poll is
  otherwise invisible, so a test wanting to act *after* the loop's clock has
  started could only guess at it from elapsed time. Sibling to
  `Camera::video_capture_starts`.
- The simulation now reproduces the SDK's **auto-exposure gain gate**: a
  freshly opened (or default-restored) camera has auto-exposure on and
  refuses a manual `Gain` write with `GeneralError` until an `Exposure`
  write with `auto = false` clears it; an `Exposure` write with
  `auto = true` turns it back on, and `control_value(Exposure).is_auto`
  reports the state. `set_control_value`'s doc describes the gate.

### Changed

- The `simulation` backend recovers from a poisoned state lock
  (`PoisonError::into_inner`) instead of panicking, matching `zwo-rs`: a
  panic on one thread no longer cascades into every later accessor call.
- `target_temperature_celsius` / `current_temperature_celsius` decode via a
  saturating `i64` -> `i32` -> `f64` conversion instead of an `as` cast;
  in-range values (all real 0.1 degC readings) are unchanged.

### Fixed

- The `simulation` backend's `get_video_data` now returns `BufferTooSmall`
  for an undersized buffer, as the SDK does, instead of panicking on the
  slice bound.
- `CameraInfo::supported_bins` now **drops** a `supported_bins` entry that is
  not a valid `u32` instead of mapping it to `0`. The `take_while(b != 0)`
  sentinel stops at a literal zero but not at a negative, so a negative entry
  became a `0` in the list — and `0` there reads as a supported bin factor. A
  driver validating a client's `BinX` against the list would have accepted `0`
  and then divided the sensor extent by it. Requires the SDK to report a
  negative bin, so no camera is known to trigger it.

### Added

- Initial repository scaffold for `svbony-rs` (safe wrapper) and
  `libsvbony-sys` (raw FFI), sibling to `qhyccd-rs` and `zwo-rs`, vendored
  first-party from day one (no external-repo detour).
- `libsvbony-sys`: hand-written `extern "C"` bindings (no bindgen — SVBony's
  SDK header carries no license text) for `SVBCameraSDK.h` (SDK 1.13.4);
  per-OS link directives for `libSVBCameraSDK` + `libusb-1.0`, gated by
  `SVBONY_SKIP_NATIVE_LINK`; no Windows support (indi-3rdparty declares it
  unsupported).
- `svbony-rs`: `Sdk` entry point + `simulation` feature; the `Camera` handle
  (open/close, enumeration with pre-open serial identity, property/capability
  queries, ROI, controls with typed gain/exposure/black-level/cooling
  wrappers, camera mode, the video-capture exposure model incl. the
  soft-trigger flow, ST4 guiding, pixel size), backing the future
  `svbony-camera` ASCOM Alpaca driver.
- Simulation backend: fabricated `SV605CC-Simulated` camera, seeded
  xorshift64 frame noise fill, a simulated soft-trigger video-capture state
  machine, and a poll-based cooling ramp.
- Dual MIT/Apache-2.0 licensing.

[Unreleased]: https://github.com/rusty-photon/rusty-photon/commits/main/crates/svbony-rs

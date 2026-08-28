# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Closing a camera can no longer free the handle out from under an SDK call in
  flight on another thread. The handle used to be copied out of its `RwLock` and
  the guard released *before* the FFI call, so `Camera::close` could run
  `CloseQHYCCD` while another thread was inside the SDK with that pointer —
  freeing the device beneath libusb, which reports it as a `usbi_mutex_lock`
  assertion and can corrupt the context every QHY device on the bus shares.
  `HandleCell::with_handle` now holds the read guard across the call, so a close
  waits for it, matching the sibling `zwo-rs`/`svbony-rs` backends. Two
  *non-close* calls still run concurrently, unchanged. Internal to the crate: no
  public API changed.

- A simulated filter wheel can no longer be commanded to a slot it cannot report.
  Writing `CONTROL_CFWPORT` decodes the value to a slot, and that decode falls
  back to a legacy decimal reading for a byte outside `0-9A-Fa-f` — so `'G'`
  became slot 23, which no hex digit names. The move was accepted, and once it
  settled *every* subsequent position read failed with `InvalidFilterSlot`,
  permanently: the wheel was parked where the read path, which reports by
  encoding the slot, had no code to answer with. The write is now rejected with
  that same error and the wheel holds its position. Simulation only, and the
  decode's fallback is unchanged on the read path, where degrading a nonstandard
  value beats failing on it.

- `SimulatedCameraConfig::with_filter_wheel` now caps the slot count at 16, the
  number a `CONTROL_CFWPORT` hex digit can address. A larger count was accepted
  and advertised through `CfwSlotsNum` and `CfwPort`'s range while every slot
  from the sixteenth up had no code to command it by, so the simulated wheel
  reported a size it then refused to move within (`InvalidFilterSlot`).
  Simulation only; configurations of 16 or fewer slots are unaffected.

- `start_single_frame_exposure` no longer reports a spurious error when
  `ExpQHYCCDSingleFrame` returns `QHYCCD_READ_DIRECTLY` (`0x2001`) — a non-error
  return meaning the single frame is already captured and can be read
  immediately. It is now accepted as success alongside `QHYCCD_SUCCESS` (matching
  INDI's indi-qhy); only `QHYCCD_ERROR` is treated as a failure. Previously the
  `check()`-based handling (== 0 only) rejected it, which would break
  single-frame capture on the cameras/modes that take that path. Affects real
  hardware only — the `simulation` backend never returns this code, so no test in
  the crate covers the branch. Adds `libqhyccd_sys::QHYCCD_READ_DIRECTLY`.
- Filter-wheel position is now encoded on `CONTROL_CFWPORT` as a **hex** ASCII
  digit (`'0'`..`'F'`, up to 16 positions), matching the QHYCCD Filter Wheel API
  and INDI's indi-qhy, instead of a decimal `+48` offset. Slots 0–9 are
  unchanged; slots ≥ 10 previously commanded/reported the wrong physical slot
  (slot 10 encoded to `':'` instead of `'A'`). Affects real hardware only (the
  simulation mirrors the crate's encoding); a new pure
  `cfw_slot_to_ascii`/`cfw_ascii_to_slot` pair is unit-tested for every slot 0–15.
- `Sdk::new` now calls `InitQHYCCD` (return-checked) before probing each camera
  for a filter wheel with `IsQHYCCDCFWPlugged`, and retries the probe up to 3×
  (200 ms apart), mirroring the reference driver: the CFW-plugged check is a live
  transaction over the camera link that `InitQHYCCD` must bring up first, so the
  previous pre-init one-shot probe could silently miss or phantom a filter wheel.
  Enumerating a camera without a filter wheel is now up to ~400 ms slower.
  Real-hardware path only.
- Documented that at most one `Sdk` may exist at a time (`InitQHYCCDResource` /
  `ReleaseQHYCCDResource` are process-global and not reference-counted) and fixed
  the `Sdk` doc example, which constructed two `Sdk` instances and so redundantly
  re-initialised / double-released the global SDK resource.
- `get_number_of_readout_modes` and `get_readout_mode_name` now treat only
  `QHYCCD_SUCCESS` as success (rejecting any other return), matching the sibling
  readout getters and INDI, instead of accepting any non-`QHYCCD_ERROR` return and
  possibly yielding a zero-initialised count / empty name.
- `get_model` and `get_readout_mode_name` decode the SDK-written name buffer with
  a bounded `CStr::from_bytes_until_nul` instead of an unbounded `CStr::from_ptr`,
  so a name the SDK writes without a NUL terminator becomes a clean error rather
  than an out-of-bounds read. The buffer is 128 bytes (matching INDI's
  `QHYReadModeInfo::label`) rather than 80.

### Changed

- **Breaking:** `set_cfw_position` rejects a slot outside the sixteen the SDK can
  address, with the new `QHYError::InvalidFilterSlot`, instead of encoding it as
  whatever byte the arithmetic produced. `CONTROL_CFWPORT` carries the position as
  a single hex digit, so slot 16 previously became `'G'` and commanded an
  undefined position; there is no code for it to have. The codec is now
  `char::from_digit` / `char::to_digit(16)` rather than hand-written offsets —
  identical on every input for the decode, and on slots 0–15 for the encode.
  Callers that bound the slot against `get_number_of_filters` (as this
  workspace's Alpaca layer does) are unaffected.
- **Breaking:** `ControlType::Other` carries a `u32` rather than an `i32`, the
  width the SDK's own `CONTROL_ID` parameter takes and the one `to_raw` already
  returned.
- **Breaking:** the colour-filter-array enum is renamed `BayerMode` →
  `BayerPattern`, matching the sibling `zwo-rs` / `svbony-rs` crates (which both
  already expose `BayerPattern`). The type name was a wrapper invention — the QHY
  SDK has no such type — so it was incidental cross-crate divergence, not
  SDK-forced. The `GBRG`..`RGGB` variants and their 1-based discriminants are
  **unchanged** (they mirror the QHY SDK's own numbering, exactly as ZWO's 0-based
  `Rg`..`Gb` mirror ASI's). The `SimulatedCameraConfig` field and helper param
  `bayer_mode` are likewise renamed to `bayer_pattern`. Update `use`s and pattern
  matches accordingly (`qhyccd_rs::BayerMode` → `qhyccd_rs::BayerPattern`).
- The simulated cooler now **ramps**: reading `CONTROL_CURTEMP` advances the
  reported sensor temperature one step (1 °C) per poll toward the `CONTROL_COOLER`
  set-point and reflects a plausible `CONTROL_CURPWM`, matching real auto-cooling
  and the sibling `svbony-rs` sim. Previously the simulated temperature was frozen
  at ambient (20 °C) and auto-cooling never populated PWM, so a "wait until cooled"
  loop never settled in simulation.
- The simulated `is_control_available` now returns `Some(0)` (`QHYCCD_SUCCESS`)
  for an available non-color control, matching the value the real arm passes
  through (was `Some(1)`); the `CamColor` bayer-code payload is unchanged.
- The simulated `close()` now clears per-session state (live-mode engagement,
  captured frame + metadata, in-flight exposure, stream mode), so a `close()` →
  `open()` cycle presents a fresh device as real `CloseQHYCCD` (which destroys the
  handle) does — previously stale state let `get_live_frame` / `get_single_frame`
  falsely succeed after a reconnect without re-arming.
- The simulated filter-wheel move is no longer instantaneous: after a set, the
  reported position converges to the target over a few polls (advance-on-poll), so
  a consumer's poll-to-arrival loop (and the ASCOM "moving" sentinel) is exercised
  as on real hardware.
- The simulated overscan area is now a distinct strip rather than a copy of the
  full effective imaging area, matching real hardware where `GetQHYCCDOverScanArea`
  and `GetQHYCCDEffectiveArea` report separate regions.
- Documented `get_live_frame`'s live-mode polling contract: it returns
  `Err(QHYError::Sdk)` both for "frame not ready" and a hard failure (the SDK does
  not distinguish them), so callers must poll/retry with a bounded budget. Added
  `SimulatedCameraConfig::with_live_not_ready_probability(p)` (default 0.0) to make
  each live read report "not ready" with probability `p`, so that poll loop can be
  exercised in simulation.
- Simulated exposures now capture their start timestamp *after* the frame is
  pre-generated, so frame-generation time no longer counts against the
  simulated exposure duration (on a loaded machine it could consume — or
  instantly complete — short exposures). Per-pixel noise in generated frames
  comes from an internal xorshift stream instead of a `rand` uniform sample
  per pixel, making simulated frame generation substantially cheaper in
  unoptimized builds; `rand` is still used for per-frame seeds and star
  placement.

### Added

- `libqhyccd-sys`'s `build.rs` honors a `QHYCCD_SDK_DIR` override on macOS
  (the directory containing `libqhyccd.a`), mirroring the existing Windows
  and Linux branches, so builds can link an SDK staged outside
  `GITHUB_WORKSPACE` / `/usr/local/lib`.
- `pub use libqhyccd_sys as sys;` re-exports the raw FFI bindings at the crate
  root (matching the sibling `zwo-rs` / `svbony-rs` convention); prefer the safe
  API in this crate.

### Changed

- **BREAKING:** error handling is now fully typed and flat, matching the sibling
  `zwo-rs` / `svbony-rs` crates. Fallible `Sdk` / `Camera` / `FilterWheel` methods
  return `qhyccd_rs::Result<T>` (`Result<T, QHYError>`) instead of `eyre::Result<T>`,
  and a public `Result<T>` alias is exported. `QHYError` is a small flat enum:
  `Sdk { op }` for any plain SDK success/fail call (the QHY ABI returns a bare
  `u32` with no discriminating error codes, so a `&'static` operation label
  replaces the former per-call-site variants), `CameraNotOpen`, the control-scoped
  `GetParameter` / `IsControlAvailable` / `GetMinMaxStep`, and the `#[from]`
  `InvalidUtf8` / `InvalidCameraId`. A public `check(status, op)` helper (the
  analogue of zwo's `asi_check` / svbony's `svb_check`) funnels the void SDK
  calls. Code that matched on `eyre::Report` — or on the former per-operation
  `QHYError` variants — must match the flat `QHYError` instead.
- Real-backend `Camera` methods return `CameraNotOpen` when called on an unopened
  camera (matching the simulation backend), rather than an operation-specific
  error.
- Target QHYCCD SDK **26.06.04**. The 26.x distribution changed packaging
  (dot-stripped repo dir `260604`, `.tar.gz` archives, no `install.sh`, and the
  per-OS archives renamed `macMix`→`mac_x64` / `WinMix`→`win64` /
  `Arm64`→`linux_arm64`). `libqhyccd-sys`'s `build.rs` now resolves the macOS
  extract dirs (`sdk_mac_arm_<ver>` / `sdk_mac_x64_<ver>`) and the Windows
  `sdk_win64_<ver>` layout accordingly; the Linux `/usr/local/lib` link path is
  unchanged. Validated on real hardware (QHY178M + 7-slot CFW, ConformU 0 errors).
- **BREAKING:** the single-frame / live-video download now writes pixels into a
  **caller-owned `&mut [u8]`** buffer and returns only the frame dimensions as
  `FrameInfo`, replacing the `Vec`-owning `ImageData` return (the `zwo-rs` /
  `svbony-rs` `download_exposure(&mut [u8])` convention). `Camera::get_single_frame`
  / `Camera::get_live_frame` take `buf: &mut [u8]` and return `Result<FrameInfo>`,
  writing the pixels into `buf`; a buffer shorter than the frame is rejected with
  the new `QHYError::BufferTooSmall { needed, got }` **before** any pixels are
  written (the sim single-frame path checks before consuming the captured image, so
  a short buffer never loses it). The public `ImageData` type is replaced by
  `FrameInfo` (dimensions only). Size the buffer with `Camera::get_image_size()`;
  no `Vec` is allocated per frame inside the library.

### Removed

- **BREAKING:** the simulated/real backend is now selected at **compile time** by
  the `simulation` feature (a per-method `#[cfg]` fork, matching `zwo-rs` /
  `svbony-rs`) instead of a runtime `CameraBackend` enum. Consequently the
  `#[automock]` FFI-mock layer (`qhyccd_rs::mocks`) and the `mockall`
  dev-dependency are gone — the real FFI arm is compiled out under `simulation`,
  so the crate no longer unit-tests through a mocked FFI (behaviour only that arm
  reached is covered by ConformU on real hardware). `Camera::new` is now available
  only *without* the `simulation` feature; build a simulated camera via
  `Camera::new_simulated` / `Sdk::new`. The `simulation/` module subtree was
  flattened into a single `src/simulation.rs` (its public API —
  `SimulatedCameraConfig` / `ImageGenerator` / `ImagePattern` — is unchanged).
- **BREAKING:** dropped the `eyre`, `educe`, and `derive_more` dependencies.
  `Camera`'s `PartialEq` (which ignores the backend handle) is hand-rolled
  (id-only) rather than derived.

### Internal

- Switched internal locks from `std::sync::RwLock` to the non-poisoning
  `parking_lot::RwLock` (the workspace standard, already used by the consuming
  camera services). Lock acquisition is now infallible, so the poison-handling
  paths and the `LockPoisoned` error variant are gone. Adds a `parking_lot`
  dependency.
- Upgraded `rand` to 0.10 (the `Rng`/`RngExt` trait split). `rand`, `rayon`,
  `thiserror`, and `tracing` now inherit the workspace dependency pins.
- Moved the demo programs from `src/bin/` to `examples/` and made
  `tracing-subscriber` a dev-dependency, so library consumers no longer pull it.
- Consolidated the six-file `camera/` module split, `backend.rs`, and `control.rs`
  into a single device-file-major `src/camera.rs` (the `zwo-rs` / `svbony-rs`
  one-file-per-device layout), with the camera's behaviour grouped into `impl`
  blocks and `ControlType` + the real-only handle machinery folded in. Purely
  internal file moves; `use qhyccd_rs::{…}` paths are unchanged.

## [0.1.9] - 2026-01-19

### Fixed

- Fixed simulation exposure cancellation bug: `stop_exposure()` now correctly preserves image data while `abort_exposure_and_readout()` discards it, matching QHYCCD SDK behavior
- Fixed double-binning bug in simulation where ROI dimensions were incorrectly divided by binning factor, causing images to be half the expected size
- Updated `get_current_image_dimensions()` to return ROI dimensions directly as they are already in binned coordinates when set via ASCOM Alpaca

### Changed

- Split simulation exposure cancellation into two distinct methods: `stop_exposure()` (preserves image) and `abort_exposure()` (discards image)
- Updated design documentation to reflect exposure cancellation behavior and ROI/binning coordinate system

## [0.1.8] - 2026-01-18

### Added

- Comprehensive design documentation for the library architecture
- Automatic default simulated camera when using `Sdk::new()` with simulation feature enabled

### Changed

- Improved simulation performance with rayon parallelization and smart waiting for exposure completion
- Refactored lib.rs into modular structure for better code organization
- Simulation feature is now transparent - `Sdk::new()` automatically provides simulated devices when simulation feature is enabled
- Updated rand dependency to 0.9.2
- Marked mock FFI functions as unsafe for better type safety

### Fixed

- Resolved simulation conformity issues with more robust testing
- Fixed cooler parameter handling bugs in simulation mode
- Removed unused imports and addressed clippy warnings

## [0.1.7] - 2025-01-01

### Changed

- **BREAKING**: Removed vendored feature from libqhyccd-sys - this change should
only affect the CI builds, as any real-world use of the library
needs the SDK installed locally
- Updated SDK version references from 24.12.26 to 25.09.29 in README
- CI/CD now uses system-installed SDK via [qhyccd-sdk-install](https://github.com/ivonnyssen/qhyccd-sdk-install) GitHub action
- Simplified build.rs to only link system libraries

### Removed

- Vendored SDK files no longer bundled with the crate
- All `--features libqhyccd-sys/vendored` flags from CI workflows

### Fixed

- Updated installation instructions in README to use correct SDK version

## [0.1.6] - Previous Release

- Previous functionality with vendored SDK support

[Unreleased]: https://github.com/rusty-photon/rusty-photon/commits/main/crates/qhyccd-rs

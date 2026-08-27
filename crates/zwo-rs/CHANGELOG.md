# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- **Breaking:** `asi_check` takes the bindgen `ASI_ERROR_CODE` alias (`c_uint`
  on LP64, `c_int` on Windows) instead of `i32`, `AsiError::from_code` takes
  `i64`, and `AsiError::Unknown` stores `i64` — a raw code outside the vendored
  header's range is preserved exactly instead of saturating into `i32::MAX`.
- **Breaking:** `ControlType::Other` stores `i64`; an unmapped control id from
  the SDK survives losslessly on either platform width of `ASI_CONTROL_TYPE`.

### Fixed

- `CameraInfo::supported_bins` now **drops** a `SupportedBins` entry that is not
  a valid `u32` instead of mapping it to `0`. The `take_while(b != 0)` sentinel
  stops at a literal zero but not at a negative, so a negative entry became a
  `0` in the list — and `0` there reads as a supported bin factor. A driver
  validating a client's `BinX` against the list would have accepted `0` and then
  divided the sensor extent by it. Requires the SDK to report a negative bin, so
  no camera is known to trigger it.
- `examples/probe_formats.rs` unpacks 16-bit pixels with `u16::from_le_bytes`
  rather than `from_ne_bytes`: the camera puts them on the wire little-endian
  regardless of host. No behavioural change on any supported target (all
  little-endian), but this probe's output is the measured evidence behind the
  `MaxADU` ceilings, and that evidence should not depend on the machine that
  gathered it.

### Added

- Initial repository scaffold for `zwo-rs` (safe wrapper) and `libzwo-sys` (raw
  FFI), sibling to `qhyccd-rs`.
- `libzwo-sys`: `bindgen`-generated bindings (build-time) from the vendored MIT
  ZWO SDK headers (`ASICamera2.h`, `EFW_filter.h`, `EAF_focuser.h`), parsed as
  C++; per-OS link directives for `libASICamera2` + `libEFWFilter` +
  `libEAFFocuser` + `libusb-1.0`.
- `zwo-rs`: `Sdk` entry point + `simulation` feature; ASI `Camera`, EFW
  `FilterWheel`, and EAF `Focuser` handles (open/close, enumeration,
  serial-derived identity, and per-device operations), backing the
  `zwo-camera` and `zwo-focuser` ASCOM Alpaca drivers.
- CI (`check`, `test`), Claude Code workflows, pre-commit hook (clippy + fmt),
  dual MIT/Apache-2.0 licensing.

[Unreleased]: https://github.com/rusty-photon/rusty-photon/commits/main/crates/zwo-rs

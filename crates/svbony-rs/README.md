# svbony-rs

Safe Rust bindings for the **SVBony camera SDK**. Sibling crate to
[`qhyccd-rs`](https://github.com/ivonnyssen/qhyccd-rs) and
[`zwo-rs`](../zwo-rs); consumed by rusty-photon's `svbony-camera` ASCOM
Alpaca driver (a later phase — see `docs/plans/archive/svbony-camera.md`).

> **Status: under construction (Phase A/B).** Enumeration, SDK-version
> queries, typed SVBony error mapping, and the camera **handle** (open/close,
> `CameraInfo` — including the serial number, which arrives at enumeration
> time — `CameraProperty`/`CameraPropertyEx`, control caps/get/set with typed
> convenience wrappers for gain/exposure/black-level/cooling, ROI, camera
> mode, the video-capture exposure model incl. the soft-trigger flow, ST4
> guiding, pixel size) are all wired to the FFI. SVBony has only one device
> (camera) and one SDK library, so — unlike `zwo-rs` — there is no per-device
> Cargo feature union.

## Crates

| Crate | Role |
|---|---|
| [`libsvbony-sys`](libsvbony-sys/) | Raw, unsafe FFI bindings. **Hand-written**, not `bindgen`-generated (see below). |
| `svbony-rs` (this crate) | The safe, ergonomic wrapper over `libsvbony-sys`. |

## Why the bindings are hand-written, not `bindgen`-generated

Unlike `libzwo-sys` (which vendors ZWO's actual MIT-licensed SDK header and
runs `bindgen` over it), `libsvbony-sys` does **not** vendor SVBony's SDK
header text. SVBony's `SVBCameraSDK.h` carries **no license text anywhere** —
not in the header itself, not in the INDI packaging that redistributes the
SDK, not in any accompanying file — so there is no written redistribution
grant for the header text. `libsvbony-sys/lib.rs` is instead
**hand-transcribed**: the `extern "C"` function signatures, struct layouts,
and named constants are reproduced from reading the header (facts like
function names, parameter order/types, and struct field order are not
copyrightable), the same posture `libqhyccd-sys` takes toward QHY's similarly
unlicensed header. See `docs/plans/archive/svbony-camera.md` ("Verified SDK ground
truth") for the full provenance trail and the source URL used.

## Enum representation

The SDK's C enums have no explicit values except where noted (SVBony's
`SVB_IMG_END`/`SVB_MODE_END` sentinels are `-1`), so ordinal position IS the
value. Rather than model them as `#[repr(<int>)]` Rust `enum`s, each is a
plain `i32` type alias with `pub const` values — mirroring `libqhyccd-sys`'s
style and sidestepping any enum-size ABI risk entirely.

## SVBony vs. QHY vs. ZWO — how this crate differs

- **License**: no grant at all (like QHY), unlike ZWO's MIT SDK header.
- **Library form**: dynamic `.so` only (`libSVBCameraSDK.so.1`, with a proper
  SONAME — unlike ZWO's SONAME-less blobs), unlike QHY's static `.a`.
- **Exposure model**: video-only. There is no snap-exposure API (`SVBGetVideoData`
  after `SVBStartVideoCapture` + `SVBSendSoftTrigger`, not a
  `StartExposure`/`GetExpStatus` pair like ASI/QHYCCD). See
  [`Camera::start_video_capture`], [`Camera::send_soft_trigger`],
  [`Camera::get_video_data`] in `src/camera.rs`.
- **Identity**: the serial number arrives at enumeration time
  (`SVB_CAMERA_INFO::CameraSN`), before the camera is opened — like QHY,
  unlike ZWO (which needs the camera open first).

## Build requirements

| Need | For | Notes |
|---|---|---|
| **The SVBony SDK library** (`libSVBCameraSDK` + **libusb-1.0**) | *linking* (`build`/`test`) | Required even with `--features simulation`, unless `SVBONY_SKIP_NATIVE_LINK=1` is set. `cargo check`/`clippy` do **not** link, so neither needs the SDK. |
| udev vendor rule | running against real hardware (Linux) | VID `f266` (SVBony's USB VID, distinct from ZWO's `03c3`). |

No libclang / bindgen requirement — `libsvbony-sys` is hand-written FFI.

Install the SDK library at `/usr/local/lib/libSVBCameraSDK.so` (or point
`SVBONY_SDK_LIB_DIR` at its directory). No packaging/provisioning action
exists yet in this repo (Phase C+ of `docs/plans/archive/svbony-camera.md`); until
then, this crate's own Bazel build (`crates/svbony-rs/libsvbony-sys/BUILD.bazel`)
bakes in `SVBONY_SKIP_NATIVE_LINK=1` so `bazel build //...`/`bazel test //...`
need zero SDK provisioning — a deliberate difference from `libqhyccd-sys`'s
and `libzwo-sys`'s Bazel targets, which link the real, pre-provisioned system
SDK.

Override the SDK lib directory with `SVBONY_SDK_LIB_DIR=/path/to/lib`; skip
all native linking with `SVBONY_SKIP_NATIVE_LINK=1`.

## Quick check (no SDK required)

```sh
cargo clippy --all --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

Neither step links, so neither needs the SVBony SDK installed.

## `simulation` feature

A hardware-free, in-Rust environment for development and tests. As in
`qhyccd-rs`/`zwo-rs`, enabling it removes the *camera*, **not** the SDK
*link* — `libsvbony-sys`'s native link directives are still emitted by
default (see "Build requirements" above for the Bazel-specific deviation).

The simulated `SV605CC-Simulated` camera models the full control set
(gain, exposure, black level, cooling) and:

- **The soft-trigger video-capture flow**: `start_video_capture` arms
  free-running (`Normal` mode, continuous) or requires an explicit
  `send_soft_trigger` per frame (`TrigSoft` mode); `get_video_data` consumes
  the armed frame or reports a timeout.
- **A cooling ramp**: `current_temperature_celsius` advances one step toward
  `target_temperature_celsius` (or back toward ambient when the cooler is
  off) **per poll** — mirroring `zwo-rs`'s EAF focuser position ramp
  (advance-on-poll, not on wall-clock time), so tests are deterministic and
  never sleep.
- **The SDK's auto-exposure gain gate**: a freshly opened camera (and one
  after `restore_default_param`) has auto-exposure on and refuses a manual
  `Gain` write with `GeneralError` until an `Exposure` write with
  `auto = false` clears it — the real SDK's behaviour, which is why a
  driver's connect handshake writes one manual exposure before any gain
  (see `Camera::set_control_value`). `set_auto_save_param` is recorded but
  the simulation persists nothing across opens.

Frames are filled with sensor noise via a seeded xorshift64 fill (the same
approach `zwo-rs`'s `fill_noise` settled on after the lessons recorded in its
doc comment — a per-byte RNG lookup and a `rayon` parallel fill both
either tripped or risked tripping ConformU's `StartExposure` timeout).

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option. This crate's own code (the
hand-transcribed FFI signatures and the safe wrapper) is ours to license;
SVBony's SDK itself carries no license grant and is never vendored or
redistributed here — only installed separately by the end user/operator.

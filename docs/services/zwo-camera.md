# Zwo-Camera Service Design

> **Status:** **Phase E (Track B full Camera) landed.** The `services/zwo-camera`
> crate now implements the full ASCOM `Device` + `Camera` surface over the
> `zwo-rs` SDK seam (`backend.rs` → production `ZwoCameraHandle` + a unit-test
> mock): connection lifecycle, sensor geometry, symmetric binning, ROI with the
> ASI `%8`/`%2` alignment rules, gain/offset, cooling, readout modes, ST4
> pulse-guiding, and the snap-mode exposure state machine (start, abort
> *discards* / graceful stop *preserves*, `ImageArray`, `CameraState`,
> `PercentCompleted`, mid-exposure `Error`), plus serial-derived identity and the
> `config.get`/`apply`/`schema` actions. The blocking SDK chain runs on
> `spawn_blocking` inside a detached task, with a generation counter +
> `result_lock` so abort/disconnect invalidate a late-completing capture; the
> camera lock is released during the integration so concurrent reads (and the
> in-flight-exposure check) are not blocked. It is gate-green: **45 unit tests**
> (against the mock seam), the **57 BDD scenarios** across the six camera feature
> files (now live), a full
> **ConformU** pass (both suites), clippy
> `--all-features`, and **Bazel** (`lib`/`binary`/
> `unit_test` are first-class `//...` targets verified on Linux; the `bdd` /
> `conformu_integration` suites run under Bazel (tagged `bdd` / `conformu`),
> mirroring qhy-camera; `MODULE.bazel.lock`
> repinned). **EFW filter wheels are out of scope** — re-planned as a future
> separate `zwo-filterwheel` service by
> [ADR-014](../decisions/014-zwo-per-device-services-and-link-features.md)
> (2026-07-10), which also narrowed this binary's native link to the camera
> SDK only (zwo-rs `camera` feature) and removed the `filterwheel.enabled`
> config toggle + `filter_names` override this doc previously specified.
>
> **ConformU — passes.** The `tests/conformu_integration.rs` harness is wired
> (gated on the `conformu` feature; skipped without `CONFORMU_PATH`). Both ConformU
> suites now pass cleanly against the simulation backend — *"no errors, warnings
> or issues found"* and *"all members returned within their target response
> times"*. Getting there took three fixes: (1) the `zwo-rs` sim now models the
> writable `Exposure` control and (2) fills the 52 MB full frame in parallel
> (rayon + bulk `RngCore::fill_bytes`, ≈11 s → ≈0.01 s, clearing the `StartExposure`
> timeout) — both in rev `3c32e59`; and two **driver** fixes surfaced once ConformU
> could finally run: (3a) `CameraXSize`/`CameraYSize` are reported *aligned* so the
> full frame at every bin (`NumX = CameraXSize/bin`) is a valid ASI ROI — the
> ASI2600's 6248 is reported as **6240** (R4), since ConformU exposes the binned
> full frame at each bin and 6248/2 = 3124 is not a multiple of 8; and (3b)
> `PulseGuide` is now **asynchronous** (returns immediately, `IsPulseGuiding`
> tracks the pulse to its deadline) instead of blocking for the pulse duration,
> which exceeded ConformU's 1 s response target. **Now also verified against real
> hardware (2026-06-20):** both suites pass on a Linux x86_64 dev box, driven
> against the production **non-`simulation`** binary (real FFI path), for two
> physical cameras — a cooled **ASI1600MM-Cool** (12-bit, `MaxADU` 4095,
> `noserial` identity fallback) and an uncooled **ASI178MM** (14-bit, `MaxADU`
> 16383, real `ASIGetSerialNumber` identity) — exercising both the cooled/uncooled
> cooler-gating split and both UniqueID paths. Since 2026-07-27 the
> ASI1600MM-Cool pass is also **recorded** in
> [docs/validation/](../validation/README.md) on both Linux **and Windows 11**
> — the Windows run being the first real-hardware exercise of the Windows
> link path. See *Testing* and *Delivery phasing*.
>
> **CI provisioning (simulation by default; real link verified nightly).** The
> `.github/actions/install-zwo-sdk` composite action provisions the SDK on
> Linux + macOS (INDI mirror) and Windows (ZWO's developer-SDK CDN zips). Because
> the cross-platform CI (`test.yml`, `conformu.yml`, `safety.yml`) all build the
> `--all-features`/`conformu` (`simulation`) path — which `cfg`s out the real FFI
> and references no SDK symbols — they set **`ZWO_SKIP_NATIVE_LINK=1`** so
> `libzwo-sys/build.rs` omits the link directives and the workspace builds with
> **no SDK to provision** (no install step, no Windows ~112 MB download). This
> also lets `safety.yml` (ASan/LSan) sanitize the simulation path instead of
> excluding the zwo crates. The **REAL** native link + FFI (`build.rs` directives,
> bindgen bindings, and the `#[cfg(not(feature = "simulation"))]` code) is built
> and link-checked on Linux/macOS/Windows — plus a Linux runtime FFI smoke — by
> the dedicated **`native.yml`** workflow (nightly + on any zwo-crate change),
> and additionally by the Bazel real variant and the Pi nightly. The aarch64
> Pi-nightly runner provisions the SDK **per run, sudo-free** — `pi-nightly.yml`
> runs `./.github/actions/install-zwo-sdk` with `sudo: "false"` (stages the blobs
> under `$RUNNER_TEMP`, symlinks the system libusb/libudev runtime libs for the
> link names, exports `ZWO_SDK_LIB_DIR` + `LD_LIBRARY_PATH`); `setup-pi-runner.sh`
> only installs the stable host prerequisites (clang/libclang, libusb-1.0
> runtime) and the udev device rule.
> The **Bazel** workflows (`bazel.yml`, `bazel-coverage.yml`) also run
> `install-zwo-sdk` — `zwo-camera` is a normal `//...` target there, like every
> workspace crate; on the Bazel build the install is unconditional (no narrowing
> job). **Remaining Track-A validation** (can't be
> exercised from a Linux dev box): confirm the macOS arm64 + Windows x64 link on
> real runners (both the Cargo *and* Bazel legs — under Bazel, the Windows
> test-runtime DLL discovery is the one unproven piece), re-run the updated
> `setup-pi-runner.sh` on the Pi, and pin the SDK refs / add download caching
> (the Windows camera zip is ~112 MB). See *Gating plan* and *Open questions*.

## Overview

The `zwo-camera` service is an ASCOM Alpaca **Camera** driver for real ZWO ASI
hardware. It exposes a connected ASI camera — exposures, ROI/binning,
gain/offset, cooling, readout, ST4 pulse-guiding — over ASCOM Alpaca on a
fixed port so the `rp` orchestrator (and any Alpaca client: NINA, SGPro,
SharpCap) can drive it like any other device. Other ZWO device families are
separate services (ADR-014): the EAF focuser is
[`zwo-focuser`](zwo-focuser.md), and the EFW filter wheel is a future
`zwo-filterwheel` service.

It is the ZWO analogue of the in-design [`qhy-camera`](qhy-camera.md) service and
reuses the same `ascom-alpaca` server framework and the
[`sky-survey-camera`](sky-survey-camera.md) (simulator) /
[`qhy-focuser`](qhy-focuser.md) (hardware driver) scaffolding.

**Provenance.** The behaviour is derived from open ZWO drivers as a *behavioural
reference only* — INDI `indi-asi`, INDIGO `ccd_asi`/`wheel_asi`, and
[`python-zwoasi`](https://github.com/stevemarple/python-zwoasi). **No code is
copied** (some references are GPL — see *Behavioural reference & licensing*),
the same clean-room discipline `qhy-camera` took toward `qhyccd-alpaca`.

**Not cross-platform.** Like `qhy-camera`, this service links a **native vendor
SDK** at compile time and is therefore gated out of the default workspace build.
See *Native dependency & build gating* — this is the dominant design constraint.

**How it differs from `qhy-camera` (drives every decision).** The `qhy-camera`
precedent assumed two things that are **both inverted** for ZWO, plus one that is
the same:

| Concern | QHY (the precedent) | ZWO (this service) |
|---|---|---|
| **SDK license** | Closed/proprietary; redistribution unresolved → authenticated/internal cache tier | **MIT** ("Copyright 2015, ZWO Company") → blob may be cached/redistributed on the **public** R2 cache mirror |
| **Rust FFI layer** | Published `qhyccd-rs`/`libqhyccd-sys` already exist; driver just writes the device layer | **No usable equivalent** → we also build & maintain `zwo-rs` + `libzwo-sys` |
| **Build/link gating** | Native lib links at compile time on *every* machine | **Same constraint**, per device feature since ADR-014 (`libzwo-sys` `build.rs` links each SDK its feature enables — this service builds with `camera` only; that SDK is required at link time even with `--features simulation`) |

Net: ZWO is **legally much easier** but **mechanically more work up front** (we
build the FFI QHY got for free). The device-trait layer is *easier* than QHY — a
cleaner C API and more ASCOM features map natively (see *ASCOM Camera surface*).
See [ADR-008](../decisions/008-zwo-camera-native-sdk-ffi.md) for the FFI-crate /
caching decision and [`docs/plans/zwo-driver.md`](../plans/zwo-driver.md) for the
full decision record.

---

## Native dependency & build gating (the crux)

This is the single most consequential fact about this service and the reason it
is delivered in two tracks.

- The imaging path is `zwo-camera → zwo-rs → libzwo-sys → ` the **ZWO ASI
  camera SDK** (`libASICamera2`, a source-less native binary) **+ libusb-1.0**.
- `libzwo-sys`'s `build.rs` emits `cargo:rustc-link-lib` per enabled device
  feature (ADR-014); this service builds `zwo-rs` with `camera` only, so the
  link is `ASICamera2` + `dylib=usb-1.0` (plus `stdc++`/`c++`) — the EFW/EAF
  SDKs are linked (and shipped) by their own services.
- **Consequence:** *every machine that compiles this package* — dev laptops, CI
  runners, Bazel actions — needs the ASI camera SDK installed and discoverable,
  plus `libusb-1.0` dev headers. Not just machines with a camera attached.
  (Bazel builds the shared `zwo-rs` targets with the union of device features,
  so Bazel actions provision all the blobs — see the ADR.)
- The `zwo-rs` **`simulation` feature** (which this service forwards as its own
  `simulation` feature) makes the build **camera-free, NOT SDK-free**: it
  fabricates fake frames (and EFW position/moving) at runtime. The native SDK is
  still required at link time. *(The ZWO SDK ships **no** simulation backend —
  unlike `qhyccd-rs` — so the simulator is wholly fabricated inside `zwo-rs`.)*

### Why this matters for rusty-photon specifically

The workspace is **100% pure-Rust at the link layer** since the `cfitsio` purge
([ADR-001 Amendment A](../decisions/001-fits-file-support.md)). `qhy-camera` is
the **first** native-SDK exception; `zwo-camera` is the **second**. The
difference is licensing: ZWO's SDK is **MIT**, so unlike the QHY blob it may live
on the **anonymous-read public** cache mirror (`cache.rustyphoton.space`) rather
than the authenticated/internal tier — the attribution notice must travel with
the cached blob. See [ADR-008](../decisions/008-zwo-camera-native-sdk-ffi.md).

### Gating plan

| Concern | Mechanism |
|---|---|
| local dev (SDK required) | `zwo-camera` is a normal workspace member but **fails to link without the SDK**. The SDK is a required local-dev prerequisite — install it (CI installs it before building); `bazel build //...` then builds the package like any other. Documented here and in the service README. |
| CI | An explicit SDK-provisioning step (`./.github/actions/install-zwo-sdk`) **pulls the pinned ASI/EFW SDK from the INDI mirror** (Linux/macOS, pinned by `ref`) **or ZWO's CDN** (Windows, unpinned "latest") + installs `libusb-1.0-0-dev`, before building/testing this package, as `bazel.yml` / `native.yml` / `scheduled.yml` do before their builds. The Linux/macOS mirror fetch is wrapped in `actions/cache` (keyed on arch + `ref`) so a warm cache skips it — the mirror otherwise rate-limits CI's ephemeral IPs (429s; see the action's own doc comment and issue #476). Required wherever the real link path is exercised; the simulation-only `test.yml` / ConformU legs build SDK-free via `ZWO_SKIP_NATIVE_LINK=1`, and `cargo check`/clippy jobs (no linker) skip it too. |
| Raspberry Pi nightly runner | **Sudo-free per-run** (the runner is sudo-less for public-repo safety): `pi-nightly.yml` runs `./.github/actions/install-zwo-sdk` with `sudo: "false"` — stages the `armv8` blobs under `$RUNNER_TEMP`, symlinks the system libusb/libudev *runtime* libs to satisfy `-lusb-1.0`/`-ludev` (no -dev package), and exports `ZWO_SDK_LIB_DIR` + `LD_LIBRARY_PATH` (the blobs carry no SONAME). `scripts/setup-pi-runner.sh` installs only the stable host prerequisites (clang/libclang-dev, libusb-1.0 runtime) — no device-access udev rule: this runner is CI-only and never has a physical camera attached, so the `99-asi.rules` rule below applies only to *production packaging* (`services/zwo-camera/pkg/`), not this runner. Mirrors the QHYCCD per-run model → self-healing for SDK bumps. **aarch64 (Pi 5) confirmed linking.** |
| Bazel | **First-class `//...` targets — no `manual` gating.** `zwo-rs` / `libzwo-sys` are **vendored first-party** ([ADR-010](../decisions/010-vendor-zwo-rs.md)), so the repo owns their `BUILD.bazel`: `libzwo-sys` is the repo's first first-party `cargo_build_script` (runs bindgen in-sandbox; its `data` carries the vendored MIT headers + `wrapper.h`), and `zwo-rs` ships **two variants** — `//crates/zwo-rs:zwo-rs` (real SDK) and `//crates/zwo-rs:zwo-rs_sim` (`testonly`, `simulation`). zwo-camera's production `lib`/`binary` link the **real** variant (the prod binary `NEEDs libASICamera2.so` — the real/sim parity win an external `@cr` crate could not give, since `crate_universe` resolves one feature set per crate); the `unit_test` / `bdd` / `conformu_integration` targets link `zwo-rs_sim`. Every workspace crate builds and tests under Bazel, so a `manual`-excluded crate would silently carry zero CI. The `bdd` / `conformu_integration` suites — which spawn the service binary (linked against `zwo-rs_sim`) and bind a port — run under Bazel, tagged `bdd` / `conformu` (mirroring qhy-camera). The `bazel.yml` / `bazel-coverage.yml` workflows run the `install-zwo-sdk` composite action (like they install OmniSim), so `libzwo-sys` links against the system SDK; `.bazelrc` forwards `LIBCLANG_PATH` (macOS bindgen) / `ZWO_SDK_LIB_DIR` (Windows link) per-OS under `strict_action_env`. zwo-camera keeps a `simulation` dev-dep with a **narrowed** role — it no longer flips a single `@cr` variant; it just keeps `simulation`'s optional deps (`rand`/`rayon`) resolved into `@cr` for the `zwo-rs_sim` target. Verified on Linux: `bazel build //crates/zwo-rs/... + //services/zwo-camera/...` and `bazel test //services/zwo-camera:zwo-camera_unit_test` (PASSED). A future hermetic `crate.annotation` (turning the SDK into a Bazel-managed `cc_import` dep, removing the imperative install) is optional cleanup, not a prerequisite. After changing the vendored crates' **external** deps, run `CARGO_BAZEL_REPIN=1 bazel mod tidy && bazel mod tidy` (Rule 10). |

### udev / USB

ZWO devices need a udev rule (`99-asi.rules`: VID `0x03c3`, `MODE=0666`,
`usbfs_memory_mb=200` for USB3 throughput). The EFW is USB-HID (no kernel
driver) but the SDK still talks libusb. macOS `.dylib`s need `install_name_tool`
fixing before linking (INDI automates this).

### Open questions still to resolve before Track A lands

1. **`zwo-rs` maturity.** The FFI crates are author-maintained and pre-1.0. They
   are now **vendored first-party** (dual-homed) at `crates/zwo-rs/`
   ([ADR-010](../decisions/010-vendor-zwo-rs.md)), so the lockstep git-rev pin is
   retired — edits are in-tree; the crates are still published to crates.io from
   the vendored subdirs for outside consumers.
2. **macOS `EFWGetNum` thread-safety.** Reportedly not thread-safe on macOS →
   enumeration is serialized (see *Concurrency*).
3. **Pi 5 aarch64 + macOS arm64 link.** The `libzwo-sys` skeleton links green on
   Linux x86_64 and locally on aarch64; CI green on Pi 5 + macOS arm64 is the
   remaining long-pole item (see *Delivery phasing* Phase A).

---

## Architecture

```mermaid
graph TD;
    A[ASCOM Client: rp / NINA / SharpCap] -->|Alpaca HTTP :11122| B[ascom-alpaca Server];
    B --> C[ZwoCameraDevice<br/>impl Device + Camera];
    C --> BB[Blocking bridge<br/>tokio::task::spawn_blocking];
    BB --> RS[zwo-rs Sdk/Camera];
    RS -->|FFI| SDK[libzwo-sys → ZWO ASI camera SDK];
    SDK -->|libusb-1.0| HW[ASI camera over USB];
    C --> CA[config_actions.rs<br/>config.get/apply/schema];
    M[main.rs<br/>ServiceRunner] --> B;
```

**Key components**

- **`main.rs`** — plain `fn main`, parses clap args, inits `tracing`, runs under
  `ServiceRunner::new("zwo-camera").with_reload().run_with_reload(...)` per
  [`service-lifecycle.md`](../skills/service-lifecycle.md). No hand-rolled signal
  handling; config bootstrap via `rusty_photon_config::resolve_and_init` with an
  **empty identity-pointer list** (identities are hardware-derived), which still
  materializes the default config file on first start.
- **`lib.rs`** — `ServerBuilder` that, on `build()`, opens the SDK and
  **enumerates every connected ASI camera**,
  registering each as an ASCOM device (index 0, 1, 2, …) with its serial-derived
  UniqueID. Because `ASIGetSerialNumber` requires an *open* camera (see *Device
  identity*), enumeration reads each camera's serial via
  `zwo_rs::Sdk::open_uninitialised` + `UninitialisedCamera::serial`
  (`ASIOpenCamera` + `ASIGetSerialNumber`/`ASIGetID` + `ASICloseCamera` — no
  `ASIInitCamera`, so this passive path touches no camera state); the eager
  per-device connect handshake, which does call `ASIInitCamera`, happens on
  `set_connected(true)`. A camera that cannot even be *opened* still fails
  enumeration outright (unchanged from before this call stopped also
  initialising) — only a post-open **identity fallback (`mint_identity`)**
  is graceful: older ASI models (e.g. the ASI1600) expose *neither* a hardware
  serial (`ASIGetSerialNumber`) nor a programmed flash ID (`ASIGetID`) — that
  read returns a general SDK error. Rather than fail the whole service, the
  camera falls back to a stable
  position-based identity (`noserial-{index}`, UniqueID
  `ZWO:{name}:noserial-{index}`), so a serial-less camera is still usable; it is
  unique per enumeration slot and stable across reconnects for the common
  single-camera case. Returns a `BoundServer`.
- **`camera.rs`** — `ZwoCameraDevice` (one instance per discovered camera)
  implementing `Device` + `Camera` against `zwo-rs`. **Every blocking SDK call
  runs inside `tokio::task::spawn_blocking`** so the async runtime is never
  stalled — *including* the CPU-heavy `to_image_array` frame transform (a
  ~26-megapixel `u16`→`i32` widen+transpose), which runs in the same
  `spawn_blocking` closure as the SDK download, never on a Tokio worker and never
  while holding `result_lock` (held only for the cheap commit).
- **`config.rs`** — typed `Config` with parse-don't-validate newtypes.
- **`config_actions.rs`** — `ConfigurableDriver` impl + the `dispatch` the
  devices delegate to (`config.get`/`config.apply`/`config.schema`).
- **`mock.rs`** (feature `simulation`/`mock`) — the hardware-free test backend
  (the `zwo-rs` `simulation` camera/EFW + a tiny in-crate trait seam over the SDK
  for unit tests).

**Concurrency.** The ASI/EFW SDKs are blocking C FFI and are **not** safe to call
from arbitrary threads concurrently for a single device. Device state (current
ROI, binning, gain, offset, target temp, exposure state machine, filter position)
is held under `parking_lot::RwLock`; all SDK calls funnel through
`spawn_blocking` and a single logical owner per device. EFW enumeration
(`EFWGetNum`) is serialized for the macOS thread-safety caveat.

The capture's integration wait (`backend.rs`) sleeps against a **real-clock
deadline** (`Instant::now() + duration`), not accumulated intended sleep time.
Under blocking-pool oversubscription — e.g. ConformU firing a storm of concurrent
property reads, each a `spawn_blocking` — individual `thread::sleep` calls
overshoot, and a loop that summed *intended* naps would run the full step count
regardless of real time, ballooning a 2 s exposure to ~10 s of wall-clock on a
contended runner (this tripped ConformU's 10 s async-operation timeout on the
macOS CI runner — a scheduling artifact, not a slow CPU). The deadline bounds the
integration to the requested duration plus at most one overshooting nap. The same
real-clock-deadline discipline is applied to every wait loop in the capture path
(integration, readout-completion poll, and the test backends), so no loop counts
fixed naps or sums *intended* sleep time. Validated under genuine concurrency:
three cameras driven by simultaneous ConformU `conformance` suites all stayed
within their response-time targets (see *Testing*).

**One stop signal per capture, one camera instance per capture.** Stopping an
exposure — `AbortExposure`, `StopExposure`, or a disconnect — is signalled
through a `StopSignal` cell that the `StartExposure` spawning that capture
creates and carries in its `CaptureRequest`, never through a cell owned by the
handle. A handle-wide cell is reset by whichever capture starts next, and a
disconnect + reconnect mid-exposure produces exactly that: the reconnect's
`reset_exposure_state` releases `exposure_in_flight` while the aborted capture is
still draining, so the next `StartExposure` is accepted and — with a shared cell —
erases the abort the disconnect had just requested. The superseded capture would
integrate on, re-lock the camera (by then the *reopened* one) and poll, download,
or `ASIStopExposure` against the exposure that replaced it. Four properties close
that window by construction rather than by timing:

- the stop cell belongs to one capture, so no capture can clear another's abort;
- `reset_exposure_state` **takes and sets** the outgoing capture's cell as it
  releases the in-flight slot, so a capture left over from a previous session
  drains within one poll step instead of running out its exposure;
- the handle stamps every open with an `open_epoch`, read by a capture under the
  same lock acquisition that starts its exposure. Both SDK calls a capture makes
  afterwards — the readout poll plus download when it re-locks the camera, and
  the `ASIStopExposure` in `stop_at_sdk` — are gated on `is_current(epoch)`,
  checked while holding that same lock. So a capture whose camera was closed and
  reopened under it issues no further SDK calls at all and reports itself
  aborted, rather than reading a frame off, or stopping, the next exposure's
  camera;
- the draining capture task releases `exposure_in_flight` only if it still *owns*
  the slot (its cell is still the installed one), so a superseded capture cannot
  declare a newer, genuinely running exposure finished and admit a third one
  alongside it.

`svbony-camera` carries the same per-capture cancel flag; ZWO's is tri-state
(none / abort / preserve) because ASI's `ASIStopExposure` also backs the
data-preserving stop (E8) that SVBony has no analogue for.

---

## MVP scope

The MVP boundary drives BDD scenario selection (Phase 2). Grounded in what the
ASI C API exposes and what `zwo-rs` will wrap.

**In scope (v0)**

- ASCOM Camera ICameraV3 for **every enumerated ASI camera** (each registered as
  a device on the one port), 16-bit (`ASI_IMG_RAW16`) monochrome **and**
  one-shot-colour (Bayer) sensors.
- Startup enumeration registers all discovered cameras;
  per-device connect/disconnect lifecycle: open → `ASIInitCamera` → RAW16
  transfer → snap mode → cache `ASI_CAMERA_INFO` (geometry, pixel size, bit
  depth, cooler/colour/ST4 flags, `ElecPerADU`) and control caps.
- Sensor geometry (`CameraXSize`/`YSize`, `PixelSizeX`/`Y`) from cached info.
  **`PixelSizeX == PixelSizeY`** trivially (ASI exposes a single `PixelSize`).
- **`ElectronsPerADU`** is a **real native value** from `ASI_CAMERA_INFO.ElecPerADU`
  (a ZWO win — QHY ships `NOT_IMPLEMENTED`).
- **Binning** — symmetric only (`CanAsymmetricBin = false`); `MaxBinX/Y` from the
  SDK's `SupportedBins`; ROI rescaled on bin change.
- **ROI** — `StartX/Y`/`NumX/Y` setters accept any `u32`; geometry validated at
  `StartExposure`, **including the ASI alignment rules**: width must be a multiple
  of 8 and height a multiple of 2. (The legacy ASI120 USB2 models additionally
  require `width·height % 1024 == 0`; `check_geometry` does **not** currently
  enforce that older-model rule — such a frame would simply be rejected by the
  SDK at `StartExposure`. Adding it is Future Work if an ASI120 USB2 is used.)
- **Exposure** — `ExposureMin/Max/Resolution` from `ASIGetControlCaps(ASI_EXPOSURE)`
  (µs; min ~32 µs for current ASI sensors — required for bias frames, see
  [`docs/workspace.md` Duration Units](../workspace.md#duration-units)); single
  `ASIStartExposure` (snap mode); `ImageReady`/`ImageArray`/`ImageArrayVariant`;
  `CameraState` (`Idle`/`Exposing`/`Error`); `PercentCompleted` from
  remaining-exposure µs.
- **Graceful stop AND abort** — `ASIStopExposure` is a single graceful,
  **data-preserving** stop ("image can still be read out"), so `CanStopExposure =
  true`; the same call backs `AbortExposure` (discarding data), so
  `CanAbortExposure = true`. *(A ZWO win — QHY ships `CanStopExposure = false`.)*
- **PulseGuide** — native `ASIPulseGuideOn/Off` (ST4), gated on the `ST4Port`
  capability → `CanPulseGuide = true` when present. *(A ZWO win — QHY defers it.)*
- **Gain / Offset** — current value + `Min`/`Max` from `ASIGetControlCaps`
  (`ASI_GAIN`, `ASI_OFFSET`/brightness); `NOT_IMPLEMENTED` if the control is
  absent on the model.
- **Readout modes = the negotiated download formats** — the camera's
  `SupportedVideoFormat` intersected with the formats this driver can deliver
  (`Raw16` first, then `Raw8`), published as `ReadoutModes` and defaulting to
  index 0. The selection drives `ASISetROIFormat`, the download buffer, the
  `ImageArray` unpack, and `MaxADU` (RM1-RM4).
- **Cooling** — `CoolerOn`, `SetCCDTemperature`, `CoolerPower`,
  `CanSetCCDTemperature`, `CanGetCoolerPower` are gated on
  `ASI_CAMERA_INFO.IsCoolerCam` (these need an actual TEC).
  **`CCDTemperature` is decoupled from cooling**: it reports the sensor
  temperature whenever the camera advertises the `ASI_TEMPERATURE` control
  (cached at the open handshake), cooled or not — most ASI cameras, including
  uncooled ones like the ASI178, expose a readable sensor temperature, and
  throwing it away as `NOT_IMPLEMENTED` would discard a genuinely useful reading.
  A camera without the control reports `NOT_IMPLEMENTED`.
- **Sensor type** — `Monochrome` vs `RGGB` (+ `BayerOffsetX/Y`) from
  `IsColorCam` / `BayerPattern`.
- **`MaxADU`** = **a saturation threshold chosen to be reachable** in the
  selected readout format, NOT `(2^BitDepth) - 1` and not an exact upper bound:
  255 in `Raw8`, and in `Raw16` the ADC's full scale *shifted into the 16-bit
  container*, one quantization step below the top code
  (`((2^BitDepth) - 2) << (16 - BitDepth)` — **65528** for a 14-bit ADC,
  **65504** for a 12-bit one). Where that margin applies, a sensor reaching its
  top code delivers pixels one step *above* `MaxADU`; ST3 explains the trade. A
  16-bit depth reports 65535 because it fills the container and there is no
  shift to step down from; an unknown depth reports 65535 because it says
  nothing about the packing at all — in both cases the value is the container's
  own maximum, so nothing can exceed it. Hardware-measured, see ST3.
  Configurable: `max_adu_reporting` selects this accurate threshold (default) or
  the flat container maximum that ZWO's own ASCOM driver reports, for clients
  written against that value — see ST3 *the compatibility switch*.
  `SensorName` comes from the device name.
- **Dark/bias frames** — ASI sensors have **no mechanical shutter**; `Light =
  false` is **accepted** and captures normally (there is no shutter to actuate —
  the frame differs only in metadata). So `HasShutter = false` and darks/bias
  work on every model (a divergence from `qhy-camera`, which rejects darks on
  shutterless models).
- `config.get`/`config.apply`/`config.schema` actions; hardware-derived
  `UniqueID` (camera SDK serial); in-process reload.
- ConformU integration test driven against the `zwo-rs` `simulation` backend
  (SDK installed in CI, no physical camera).

**Deferred (see *Future Work*)**

- **Video mode** (`ASIStartVideoCapture`) — the high-FPS guiding/planetary path;
  v0 is snap-mode only (snap and video are mutually exclusive).
- **EFW filter wheel** and **EAF focuser** — separate services per ADR-014:
  the EAF is [`zwo-focuser`](zwo-focuser.md) (landed); the EFW is a future
  `zwo-filterwheel` service (`docs/plans/zwo-driver.md` Phase F).
- **CAA rotator** (`CAA_API.h`) — only if a ZWO rotator is ever in scope.
- Per-serial connect-time tuning (gain/offset/target-temperature defaults).
- `FullWellCapacity` (no native ASI field; supply a placeholder only if ConformU
  requires it).
- **Vendoring the SDK** into `libzwo-sys` (MIT permits) to drop external
  provisioning — deferred in favour of mirroring `qhyccd-rs`'s external model.

---

## Configuration

The service **enumerates every connected ASI camera** at startup and registers
each as an ASCOM device (camera index 0, 1, 2, …) on the one port. The hardware
is the source of truth — there is no per-camera *binding* in config. Each
device's UniqueID comes from its SDK serial; config carries only optional
per-serial display overrides plus the port.

```jsonc
{
  // Optional per-device overrides, keyed by SDK serial. A device with no entry
  // uses SDK-derived defaults (name from model+serial). Named `devices` (not
  // `overrides`) to avoid colliding with the config.get response's own
  // `overrides[]` (CLI-pinned paths) field.
  "devices": {
    "ASI2600MM-0A1B2C3D4E5F6071": {
      "name": "Main Imaging",
      "description": "ASI2600MM-Pro @ 1000mm"
    }
  },
  // Which MaxADU contract to present (ST3). Omit for the accurate default.
  "max_adu_reporting": "saturation_threshold",
  "server": {
    "port": 11122,
    "bind_address": "0.0.0.0",
    "tls": null,
    "auth": null
  }
}
```

The `server` block is the shared `AlpacaServerConfig` from
`crates/rusty-photon-server-config` (see ADR-016): `port`, `bind_address`
(default `0.0.0.0`), optional `discovery_port`, and optional `tls`/`auth`.
Absent `tls`/`auth` means plain, unauthenticated HTTP.

Sections:

- **devices** — Optional per-device override map keyed by **SDK serial** (the
  16-hex `ASIGetSerialNumber` value). Lets an operator give a friendly
  `name`/`description` to a specific camera. Any device without an entry uses
  SDK-derived defaults. v0 does **not** carry per-camera connect-time tuning
  (gain/offset/target temperature) — with heterogeneous cameras those are
  per-serial concerns and clients set them over ASCOM; per-serial defaults are
  deferred (see *Future Work*).
- **max_adu_reporting** — Which `MaxADU` contract to present, service-wide:
  `saturation_threshold` (default, the accurate reachable value) or
  `container_full_scale` (a flat 65535 in `Raw16`, matching ZWO's own ASCOM
  driver for clients written against it). Editable, but baked into each device
  at construction, so it takes effect on the reload `config.apply` fires rather
  than on the live objects. See ST3 *the compatibility switch* — the compat mode
  disables saturation detection on sub-16-bit sensors.
- **server.port** — Listening port (**11122**, next free in the 1112x family;
  11121 is `qhy-camera`). One port hosts all enumerated devices. Hard read-only
  (self-lockout: a port change would make the BFF lose the devices).

*(The former `filterwheel.enabled` toggle and per-serial `filter_names`
override left with the EFW re-scope — ADR-014; they will reappear in the
future `zwo-filterwheel` service's own config.)*

### Config actions

Standard cross-driver protocol ([`config-actions.md`](config-actions.md)),
implemented generically in `rusty_photon_config::actions` + the ASCOM adapter in
[`rusty-photon-driver`](../../crates/rusty-photon-driver). `config_actions.rs`
supplies `ConfigurableDriver for ZwoCameraDriver`:

- **Secrets redacted/carried forward:** `server.auth.password_hash` (the one
  secret; `server.tls` stores file *paths*, not key material).
- **Locked (identity) fields:** none — UniqueIDs are hardware-derived and not
  stored in config, so there is no identity field to lock (a deliberate
  divergence from the minted-identity convention; see *Device identity*).
- **Hard read-only fields:** `/server/port` (self-lockout — a BFF could not
  follow the rebind).
- **Editable fields:** the `devices` map (per-serial `name` / `description`).
- **Validation** at load (parse-don't-validate): `devices` keys are free-form
  serial strings and the override values are free-form display strings, so v0
  has nothing semantic to validate — but unknown keys are **rejected at
  deserialize** (`deny_unknown_fields`, as in zwo-focuser), so typos and
  removed keys (notably a pre-ADR-014 `filterwheel` section) fail loudly at
  load instead of being silently ignored.

`config.apply` persists atomically, returns `status:"applying"` when a field
changed, and fires the in-process reload (`main.rs` runs under
`with_reload().run_with_reload(...)`).

### Device identity (UniqueID)

ASCOM requires a globally-unique, never-changing `UniqueID`. **This service
derives the UniqueID from the camera's hardware serial** — the same scheme as
`qhy-camera`.

A **ZWO-specific wrinkle:** `ASIGetSerialNumber` (the stable 8-byte → 16-hex id,
available only since ASI SDK driver V1.14.0227) requires the camera to be
**opened first** — unlike QHY's pre-open read. So enumeration opens each camera
briefly via `zwo_rs::Sdk::open_uninitialised` (`ASIOpenCamera` + `ASIGetSerialNumber`
— never `ASIInitCamera`, see C5) to read its serial, then closes it. The fallback
chain is:

1. `ASIGetSerialNumber` (open briefly → read → close) — the canonical identity.
2. `ASIGetID` (a writable, USB3-only flash id) — a weak fallback for older
   cameras that report no serial.
3. Otherwise (`mint_identity`) a stable position-based identity,
   `noserial-{index}` — see the `lib.rs` component description above.

Consequences (same as
`qhy-camera`): **no `unique_id` field in config**, an **empty identity-pointer
list** passed to `resolve_and_init` in `main.rs` (no minting; the bootstrap
still materializes the default config file on first start), and **no locked
identity field** in the config-actions tiers. Two identical-model cameras are
naturally distinguished by serial.

---

## Behavioral contracts

Named, testable behaviours, each mapping to a BDD scenario in `tests/features/`
except where a contract notes a unit-tested branch (e.g. E9, and PG2's no-ST4
path). ASCOM error names per [`docs/references/ascom-alpaca.md`](../references/ascom-alpaca.md).
Values are grounded in the `zwo-rs`-backed implementation; the `simulation`
backend presents one **ASI2600MM-Pro-Simulated** camera (6248×4176, monochrome,
16-bit, cooled, ST4 present). (`zwo-rs`'s sim also fabricates an EFW and an
EAF; those belong to the other zwo services.)

> The simulator's capability set (cooler + ST4 + 16-bit) is chosen so the BDD
> suite exercises the **full** ASCOM surface from a single device. ST4 on a
> 2600-class body is a simulator convenience, not a shipping-SKU claim; the
> `simulation` backend is wholly fabricated inside `zwo-rs` (the ASI SDK has no
> simulation mode).

### Enumeration & connection lifecycle

- **C0.** At startup `build()` enumerates all connected ASI cameras
  and registers each as an ASCOM device with its
  serial-derived UniqueID (opening each camera briefly via
  `Sdk::open_uninitialised` to read the serial, without initialising it — see
  C5). Zero discovered cameras is **not** a hard failure — the service starts
  with no Camera devices, logged at `warn!`; a later reload re-enumerates. A
  camera that cannot even be *opened* for its serial read (e.g. removed
  between `Sdk::cameras()` and the open, or claimed by another process) **is**
  a hard failure for the whole enumeration (and so for `build()`/reload) —
  only a *post-open* read failure (no serial, no flash id) is caught and
  downgraded to the `noserial-{index}` fallback (see *Device identity*).
- **C1.** `set_connected(true)` on a device opens *that* camera, `ASIInitCamera`,
  selects RAW16, snap mode, and caches `ASI_CAMERA_INFO`, supported binning modes,
  and exposure/gain/offset control caps. On success `Connected = true`.
- **C2.** `set_connected(true)` with the device's camera unreachable / SDK open
  failure returns the mapped driver error and `Connected` stays `false`.
- **C3.** `set_connected(false)` closes that device and returns `NOT_CONNECTED`
  for subsequent operations; an in-flight exposure on it is aborted. A reconnect
  landing while that capture is still draining is E10.
- **C4.** Connect is per-device and independent: connecting/disconnecting one
  camera does not affect the others enumerated on the same service.
- **C5.** No code path in this service pushes cooler state or any other
  actuation on startup, connect, or `config.apply` (workspace tenet
  [*no actuation on connect*](../workspace.md#project-tenets)); cooler commands
  are issued only by explicit ASCOM setters, and no cooler setpoint is restored
  on connect. The C0 serial read runs only `ASIOpenCamera` +
  `ASIGetSerialNumber`/`ASIGetID` + `ASICloseCamera`
  (`zwo_rs::Sdk::open_uninitialised` + `UninitialisedCamera::serial`)
  on every camera at *service start* and on every `config.apply` reload —
  `ASIOpenCamera` is documented by the vendor SDK as not affecting a capturing
  camera. It deliberately never calls `ASIInitCamera` (which resets controls,
  e.g. the cooler, to SDK defaults); that call is reserved for the per-device
  `set_connected(true)` handshake (C1), so startup and reload touch no camera
  state. (Resolved issue #637; previously this path ran `ASIInitCamera` on
  every enumerated camera at startup.)

### Geometry, binning, ROI

- **G1.** `CameraXSize`/`CameraYSize`/`PixelSizeX`/`PixelSizeY` reflect the cached
  `ASI_CAMERA_INFO`; `PixelSizeX == PixelSizeY` (single SDK `PixelSize`).
- **R4.** `CameraXSize`/`CameraYSize` are reported *aligned down* so the full
  frame at every supported bin — `NumX = CameraXSize / bin`, the ROI conformance
  tools and clients expose at each bin — is a valid ASI ROI (binned width a
  multiple of 8, binned height a multiple of 2). The reported extent is the
  largest multiple of `lcm(unit · bin)` (unit = 8 for X, 2 for Y) not exceeding
  the raw sensor; for the ASI2600 (6248×4176, bins 1–4) that is **6240×4176**
  (the raw 6248/2 = 3124 is not a multiple of 8, so the raw width would make the
  bin-2/3/4 full frames unachievable). The cost is a few edge columns at full
  resolution; the bonus is that the bin-ratio ROI rescale (B3) round-trips
  exactly. Bounds checks (R2) use the *reported* extent. Both extents are computed by
  [`rusty-photon-camera-core`](../../crates/rusty-photon-camera-core/)'s
  `aligned_sensor` from the *same* alignment rule R3 validates against, so the
  reported size and the ROI check cannot be aligned to different multiples.
  Shared with the sibling camera driver that hit the same ConformU failure on
  different hardware.
- **B1.** `set_bin_x`/`set_bin_y` validate against the SDK's `SupportedBins` and
  set symmetric binning; an unsupported bin returns `INVALID_VALUE`.
- **B2.** `CanAsymmetricBin = false`; `MaxBinX`/`MaxBinY` come from
  `SupportedBins` (typically 1–4, up to 8).
- **B3.** A bin change rescales the cached ROI by the bin ratio. `set_num_x`/
  `set_num_y` store without validating (the members are set independently, so
  only the combination is checked, at `StartExposure`), so whatever the client
  last set is what gets rescaled — and the rescale must not change which value
  `StartExposure` then complains about. A **sub-pixel** extent is clamped to a
  minimum of 1, because truncating it to 0 would make R2 reject a value the
  driver invented rather than the client's own `NumX`, which here is R3's
  `%8`/`%2` rule. A **client-set 0** is preserved, so it still earns R2 rather
  than being clamped into an R3 alignment complaint about a 1 nobody set.
  **One implementation**, in
  [`rusty-photon-camera-core`](../../crates/rusty-photon-camera-core/) — this
  rule was three copies until one drifted, and the drift went unseen because
  each driver curated its own test cases, so the missing behaviour and its
  missing test hid each other.
- **R1.** `StartX/Y`/`NumX/Y` setters accept any `u32`; geometry is validated at
  `StartExposure` (R2/R3), not at the setter.
- **R2.** `StartExposure` with `StartX + NumX > CameraXSize / BinX` (or the Y
  analogue), or `NumX/NumY = 0`, returns `INVALID_VALUE`.
- **R3.** `StartExposure` with a sub-frame that violates the ASI alignment rules —
  `NumX % 8 != 0` or `NumY % 2 != 0` — returns `INVALID_VALUE`; otherwise the ROI
  is applied to the SDK before exposing.
- **R-order.** When a ROI breaks more than one rule at once, the client is told
  about the first of: zero extent, zero bin, alignment, bounds. The order is part of
  the contract and is pinned by tests in
  [`rusty-photon-camera-core`](../../crates/rusty-photon-camera-core/),
  because it decides which value a client is sent to fix — a zero bin is not a
  geometry that fails a rule but one with *no rule to apply*, so it is reported
  ahead of a complaint a client could otherwise chase while the real problem sat
  in `BinX`.

### Exposure

- **E1.** `StartExposure` while disconnected returns `NOT_CONNECTED`.
- **E2.** `StartExposure` while exposing returns `INVALID_OPERATION`.
- **E3.** `StartExposure` `Duration` outside `[ExposureMin, ExposureMax]` returns
  `INVALID_VALUE`.
- **E4.** `StartExposure` with `Light = false` (dark/bias) is **accepted** on
  every model: ASI cameras have no mechanical shutter, so the frame is captured
  identically and differs only in client-applied metadata. `HasShutter = false`.
- **E5.** A successful `StartExposure` sets exposure µs, runs the ASI single-frame
  capture on the blocking bridge, and on completion produces an `ImageArray` of
  the binned sub-frame, `ImageReady = true`,
  `LastExposureStartTime`/`LastExposureDuration` set, `CameraState = Idle`.
- **E6.** `CameraState` is `Exposing` during capture; `PercentCompleted` is
  derived from remaining-exposure µs (clamped to ≤ 100), `100` once ready.
- **E7.** `AbortExposure` during capture calls `ASIStopExposure` and discards the
  frame, leaving `ImageReady = false`; `CanAbortExposure = true`.
- **E8.** `StopExposure` during capture calls `ASIStopExposure` and **preserves**
  the partially-integrated frame ("can still be read out"); `CanStopExposure =
  true`. *(The ZWO inversion of `qhy-camera` E8.)*
- **E9.** A mid-exposure SDK error transitions `CameraState = Error`, sets
  `last_error`, leaves `ImageReady = false`, logged at `warn!`.
- **E10.** A disconnect and reconnect *during* an exposure aborts that capture
  and leaves the reconnected device `Idle`; the next `StartExposure` is accepted
  and returns its own frame. The superseded capture — which may still be draining
  its un-interruptible SDK chain — can neither cancel, consume, nor stop the new
  exposure, and does not release its in-flight slot: its stop signal is its own,
  and every SDK call it makes is gated on the camera instance it started
  (*Concurrency*, "One stop signal per capture"). Its own result is discarded by
  the generation guard.

### Gain / offset / readout

- **GO1.** `Gain`/`Offset` return the current SDK value, or `NOT_IMPLEMENTED` if
  the control is unavailable on the model. The SDK reports it as a `long`; a
  value outside ASCOM's `i32` returns `INVALID_OPERATION` rather than a
  truncated number.
- **GO2.** `set_gain`/`set_offset` validate against cached `[min, max]` and apply
  via the SDK; out-of-range returns `INVALID_VALUE`.
- **GO3.** `GainMin/Max`, `OffsetMin/Max` reflect the cached SDK min-max,
  converted **once at the open handshake** from the SDK's `long` to ASCOM's
  `i32`. A bound with no `i32` spelling leaves the control **unadvertised**
  (`NOT_IMPLEMENTED` from all four members) rather than advertising a clamped
  bound the camera would then reject.
- **GO4.** The cache is the sole gate on all six members, so each connect
  **overwrites** it — including with "unavailable". Here that falls out of the
  handshake assigning it unconditionally (`find(ControlType::Gain).and_then(…)`,
  which yields `None` when the control is absent), so a control missing on this
  connect cannot leave a previous session's bounds standing to be advertised.
  Identical in `svbony-camera` and `qhy-camera`, which reaches it differently —
  see its GO4.
- **RM1.** `ReadoutModes` is the camera's **download-format** list: at
  enumeration the driver intersects `ASI_CAMERA_INFO.SupportedVideoFormat` with
  the formats it can deliver, in preference order `Raw16` then `Raw8`, and
  publishes the survivors (`["Raw16", "Raw8"]` on every model measured so far).
  `ReadoutMode` defaults to index 0 — the highest precision the camera offers —
  and resets to it on every connect. `set_readout_mode` validates the index;
  out of range → `INVALID_VALUE`. Changing it while an exposure is in flight →
  `INVALID_OPERATION`, so the delivered frame and the `MaxADU` describing it
  can never disagree.
- **RM2.** The selected mode is the driver's whole format story: it is what
  `ASISetROIFormat` receives, what sizes the download buffer
  (`w × h × bytes_per_pixel`), which unpack `ImageArray` uses (1 or 2 bytes per
  pixel), and what `MaxADU` reports (ST3).
- **RM3.** A camera advertising **neither** `Raw16` nor `Raw8` fails connect
  with `NOT_CONNECTED`, the advertised list logged at `warn!`. No such ASI model
  is known — `RAW8` is the SDK's universal baseline — but failing loudly beats
  silently downloading something the rest of the driver does not describe.
- **RM4 (deliberate exclusions, and why INDI's model guard is not ported).**
  `RGB24` and `Y8` are not eligible: `RGB24` is SDK-debayered
  8-bit-per-channel output that would change the *device* contract
  (`SensorType::Color`, no `BayerOffset`, a rank-3 `ImageArray`) rather than
  just the buffer arithmetic, and `Y8` is a luminance format — redundant with
  `Raw8` on a mono sensor and wrong on a Bayer one while we report `RGGB`.
  **`indi-asi` additionally refuses `RAW16` on any device whose name contains
  `ASI120`/`ASI130`, and that guard is deliberately not ported**: measured on a
  physical **ASI120MC-S**, which advertises `[Raw8, Rgb24, Y8, Raw16]` *and*
  delivers `Raw16` correctly at 320×240, full-frame 1280×960 and bin 2 (see
  "Real-hardware validation"). A name match would force that working camera to
  8 bits. If a USB2 ASI120/ASI130 is ever shown to misbehave, the guard can be
  added then — scoped by `is_usb3`, and with evidence.
- **RM5.** The `ImageArray` unpack is total in both directions, and reports the
  **format before the length**: a format the driver cannot unpack (RM4) is
  rejected as such even when the buffer is also short, because the length it
  would be measured against is derived from that same unusable format. A buffer
  shorter than `w × h × bytes_per_pixel` is rejected as "buffer too small". The
  8-bit path takes the download buffer **by value** and hands it to `Array2`
  without copying; 16-bit pays one, since its bytes must be re-read as `u16`.
  **One implementation**, in
  [`rusty-photon-camera-core`](../../crates/rusty-photon-camera-core/) — this
  driver's share is only which of its own formats maps onto which pixel
  depth, and the format name the message carries.

### Cooling

- **K1.** `CanSetCCDTemperature` / `CanGetCoolerPower` are `true` iff
  `ASI_CAMERA_INFO.IsCoolerCam`; otherwise the related getters return
  `NOT_IMPLEMENTED`.
- **K2.** `CCDTemperature` returns the current sensor temperature
  (`ASI_TEMPERATURE`, 0.1 °C units) when cooling is supported.
- **K3.** `set_set_ccd_temperature` validates `[-273.15, 80]` and sets the target;
  `SetCCDTemperature` reads it back.
- **K4.** `CoolerOn`/`set_cooler_on` map to `ASI_COOLER_ON`; `CoolerPower` is the
  normalized `ASI_COOLER_POWER_PERC` percent.

### Sensor type & signal

- **ST1.** `SensorType` is `RGGB` (colour) when `IsColorCam`, else `Monochrome`;
  `BayerOffsetX/Y` follow `ASI_CAMERA_INFO.BayerPattern`. The driver maps
  `ASI_BAYER_*` — which abbreviates the quad to its first row — onto
  [`rusty-photon-camera-core`](../../crates/rusty-photon-camera-core/)'s
  `BayerPattern`, which locates the first red photosite; the offsets themselves
  are **one implementation** across the three camera drivers.
- **ST2.** `ElectronsPerADU` returns the native `ASI_CAMERA_INFO.ElecPerADU`
  (a finite positive value), **not** `NOT_IMPLEMENTED` — read **live on every
  call**, never from the `CameraInfo` cached at enumeration and never computed.
  The SDK scales this field by the gain register, by a law that **differs per
  model** (see *`ElecPerADU` is gain-scaled* below). A cached value would freeze
  the property at whatever gain the camera happened to hold when the service
  enumerated it and would not move when a client changes `Gain` — which is
  precisely what a client reading `ElectronsPerADU` for SNR or exposure math
  needs it to do.
- **ST3.** `MaxADU` = **a saturation threshold chosen to be reachable** by the
  delivered data in the selected readout mode (RM2) — not `(2^BitDepth) - 1`,
  and deliberately *not* an exact upper bound on the pixel values (see *the
  margin*, which explains why a client may see values slightly above it, and
  why "reachable" is a design intent rather than a guarantee):
  - `Raw8` → **255**, whatever the ADC depth is.
  - `Raw16` → `((2^BitDepth) - 2) << (16 - BitDepth)`: **65528** for a 14-bit
    ADC, **65504** for a 12-bit one — one quantization step below the shifted
    full scale, see *the margin* below.
  - `Raw16` from a 16-bit ADC → the container's own **65535**: it fills the
    container, so there is no shift to step down from.
  - `Raw16` from an unknown (0) or degenerate (1) depth → **65535** as well,
    but for a different reason: the depth says nothing about the packing, so
    there is no step size to step down by.

  ASI packs sub-16-bit ADC data into the Raw16 container by *left-shifting* it,
  so the ceiling belongs to the container, not the ADC. Hardware-measured on a
  12-bit ASI120MC-S: every pixel's low 4 bits are zero and a saturated full
  frame tops out at exactly `4095 << 4 = 65520` — sixteen times the 4095 this
  driver used to report, so any client normalising by `MaxADU` mis-scaled
  everything above 1/16 of range. The shift — rather than a rescale — is
  **confirmed independently on a 14-bit ASI178MM**, whose pixels carry two
  always-zero low bits at bin 1, so `16 - BitDepth` is a real packing rule and
  not an extrapolation from one camera. (`svbony-camera` reached the same
  *conclusion* on its SV605CC by the opposite mechanism — there a genuine
  rescale, low bits populated.) An unknown (0) depth falls back to the
  container's own 65535.

  **The margin: why the shifted branch reports one step below full scale.**
  A sensor need not reach its top ADC code. The physical ASI178MM clips at
  `16382 << 2` = **65528**, one count short of the `16383 << 2 = 65532` the
  shift alone predicts — measured at every gain from 0 to 510, at bins 1-3,
  and at exposures from 1 s to 15 s, with ~98 000 pixels piled on 65528 and
  nothing above it. ASCOM defines this property as *"the maximum ADU value the
  camera can produce"*, and clients test saturation as `pixel >= MaxADU`, so an
  unreachable ceiling does not merely round badly: it makes saturation
  **undetectable**. Measured through the driver before the margin existed, a
  comprehensively blown-out frame gave `pixels >= MaxADU` = 0 while 13 655
  pixels sat at the sensor's real ceiling; with it, the same frame reports
  6 709 saturated pixels.

  **The shortfall is genuinely per-model — all three cameras were blown out
  and they do not agree:**

  | camera | ADC | shift predicts | **delivered ceiling** | margin |
  |---|---|---|---|---|
  | ASI178MM | 14-bit | 65532 | **65528** = `16382 << 2` | exact |
  | ASI1600MM-Cool | 12-bit | 65520 | **65504** = `4094 << 4` | exact |
  | ASI120MC-S | 12-bit | 65520 | **65520** = `4095 << 4` | costs one code |

  Two of the three clip one ADC count short, and on those the margin *is* the
  measured ceiling rather than a conservative approximation. The ASI120MC-S
  really does reach full scale — confirmed at every gain across its 0-100
  range — so identical 12-bit sensors disagree, and no formula derived from
  `BitDepth` can be exact on all of them.

  **All three ceilings are confirmed through a second, independent driver
  stack.** Driving the same three cameras through **ZWO's own ASCOM driver**
  (6.5.36, ASCOM Platform 7.1.3, Windows) to saturation delivers 65528, 65504
  and 65520 — identical to the values above, measured by different code, in a
  different language, on a different operating system, against a different build
  of the SDK. The per-model shortfall is therefore a property of the sensors,
  not an artefact of this driver's unpacking.

  **One step is what the measurements support, not a proof.** Every sensor seen
  so far clips by at most one ADC count, so one step of margin is enough to
  make `pixel >= MaxADU` satisfiable on all of them. A model that clipped
  *two* or more counts short would defeat it again, and nothing in the SDK
  would reveal that in advance — so on an unmeasured camera, reachability is
  the design intent rather than a guarantee. Widening the margin further is not
  free: each extra step raises the false-positive band on every sensor that
  does reach its top code, which is why it is one and not two. Any new model
  worth trusting for saturation detection should be run through
  `probe_ceiling` (or `probe_gain_sweep` for the whole register).

  **The cost of being wrong in the safe direction is measured, and it is
  tiny.** On the ASI120MC-S, a blown-out full frame through the driver:

  ```
  delivered max : 65520  (6095 px)      advertised MaxADU : 65504
  pixels >= MaxADU : 6098   of which at the ceiling: 6095   false positives: 3
  ```

  Three pixels out of 1 228 800 — 0.0002% — are called saturated one ADC LSB
  early, while all 6 095 genuinely saturated pixels are flagged. Against that,
  without the margin the other two cameras report **zero** saturated pixels on
  a comprehensively saturated frame. The error is *below the sensor's own
  resolution* either way, since a shifted container cannot represent anything
  finer; the asymmetry is that understating costs a fraction of one code while
  overstating costs the entire capability.

  **The margin's other consequence: on a sensor that reaches full scale,
  delivered pixels exceed the advertised `MaxADU`.** The ASI120MC-S delivers
  65520 while reporting 65504, so a client normalising `pixel / MaxADU` sees
  **1.00024** at the top codes rather than 1.0. Measured across that camera's
  whole gain register, this happens at **101 of 101 gains** — it is the normal
  case on such a sensor, not an edge case. Clients that clamp are unaffected;
  one that asserts `≤ 1.0` would trip. That is the price of choosing a
  reachable saturation threshold over an exact upper bound, and ASCOM's single
  `MaxADU` cannot express both.

  **The compatibility switch: `max_adu_reporting`.** ZWO ships its own ASCOM
  driver for these cameras, and it reports a flat **65535** — measured on all
  three, driver 6.5.36 under ASCOM Platform 7.1.3, constant across every readout
  mode, five gains spanning each camera's full range, and every supported bin.
  Clients written against ZWO's driver may therefore carry logic that assumes
  65535, and for them this driver's accurate value is a behaviour change: pixels
  never reach `MaxADU` on such a client's arithmetic *by design*, whereas here
  they do, and on a sensor that reaches full scale they exceed it. A top-level
  config key chooses which contract to present:

  | `max_adu_reporting` | `Raw16`, sub-16-bit ADC | meaning |
  |---|---|---|
  | `saturation_threshold` *(default)* | `((2^BitDepth) - 2) << (16 - BitDepth)` | reachable — `pixel >= MaxADU` detects saturation |
  | `container_full_scale` | 65535 | matches ZWO's own ASCOM driver; nothing can exceed it |

  The setting is **service-wide** and changes only the shifted branch: `Raw8` is
  255 either way, and a 16-bit or unknown depth is 65535 either way, because in
  those branches the container maximum *is* the answer. The default is the
  accurate value — saturation detection is the capability the property exists
  for, and it should work without configuration; `container_full_scale` is the
  deliberate opt-out for an installation whose client needs ZWO's number.

  Choosing `container_full_scale` **disables saturation detection** on any
  sub-16-bit sensor: that is precisely the state ZWO's driver is in, where a
  fully blown-out frame reports zero pixels at or above `MaxADU` on all three
  cameras. It is a compatibility mode, not a second correct answer.

  **Verified exhaustively, at every gain each camera advertises** — 601, 511
  and 101 values respectively, 1 213 frames, exposure scaled by `10^(gain/200)`
  so low gains are not left dark (an unsaturated frame cannot falsify a
  ceiling):

  | camera | gains | ceiling delivered | exceeded it | reached it | packing |
  |---|---|---|---|---|---|
  | ASI1600MM-Cool | 601 | 65504 | never | 586/601 | shift 4 at every gain |
  | ASI178MM | 511 | 65528 | never | 511/511 | shift 2 at every gain |
  | ASI120MC-S | 101 | 65520 | never | 101/101 | shift 4 at every gain |

  No frame at any gain on any camera exceeded its ceiling, and the shift
  signature never moved. The 15 ASI1600MM-Cool gains that did not *reach* the
  ceiling are exactly gains 0-14, where the brightest pixel climbs 55696 →
  65008 and then pins from gain 15 up — an under-exposed scene at the bottom of
  the register, not a second ceiling. `crates/zwo-rs/examples/probe_gain_sweep.rs`
  is the probe; it runs for about an hour across three cameras.

  The margin is spent only where the shift creates it (see the ST3 bullets
  above): a 16-bit ADC fills the container and has no shift to step down from,
  while an unknown depth says nothing about the packing at all. `Raw8` was
  measured reaching exactly 255 on every camera tried, so it takes no margin
  either.

  One further measured caveat, which the formula does not express:

  - **The shift signature is a bin-1 property.** At bin ≥ 2 the SDK combines
    neighbouring ADC counts, which populates the low bits — an ASI178MM frame
    at bin 2 looks "rescaled" by the low-bit test. The *ceiling* is unchanged,
    so the single published `MaxADU` still describes every bin.

### Pulse guiding

- **PG1.** `CanPulseGuide` is `true` iff the camera reports an ST4 port; the
  simulated camera reports ST4 present.
- **PG2.** `PulseGuide(direction, duration)` is **asynchronous**: it starts the
  ST4 pulse (`ASIPulseGuideOn`) and returns immediately, with `IsPulseGuiding`
  reporting `true` until the pulse's deadline (`now + duration`); a detached task
  ends it (`ASIPulseGuideOff`) when the deadline passes. Blocking for the whole
  pulse would exceed ConformU's 1 s response target and stall an autoguider's
  cadence. While disconnected it returns `NOT_CONNECTED`; a model without ST4
  returns `NOT_IMPLEMENTED`. *(The disconnected branch is a BDD scenario; the
  no-ST4 `NOT_IMPLEMENTED` branch and the async `IsPulseGuiding` timing are
  covered by unit tests, since the `simulation` backend always reports ST4
  present.)*

### FilterWheel — moved to the future `zwo-filterwheel` service

The FW1–FW3 contracts (Names/Position with the `-1` moving sentinel,
`set_position` range validation, zero `FocusOffsets`) that this section
previously specified move verbatim to the future separate `zwo-filterwheel`
service (ADR-014; `docs/plans/zwo-driver.md` Phase F), along with the
`filter_names` overrides and the removed `@wip` `filter_wheel.feature`
scenarios.

---

## ASCOM Camera surface — v0 behaviour

| Property / Method | v0 behaviour (backed by `zwo-rs`) |
|---|---|
| `CameraXSize` / `CameraYSize` | Cached `ASI_CAMERA_INFO` MaxWidth/MaxHeight, aligned down so the full frame at every bin is a valid ASI ROI (R4; e.g. 6248→6240) |
| `PixelSizeX` / `PixelSizeY` | Cached `ASI_CAMERA_INFO.PixelSize` (X == Y) |
| `BinX` / `BinY` / `MaxBinX` / `MaxBinY` | Symmetric; max from `SupportedBins` |
| `CanAsymmetricBin` | `false` |
| `NumX` / `NumY` / `StartX` / `StartY` | Setters relaxed; validated at `StartExposure` (incl. %8 / %2) |
| `MaxADU` | A saturation threshold chosen to be reachable, not an exact upper bound (ST3): 255 in Raw8; in Raw16 the ADC scale shifted into the container, one quantization step below full scale — 65528 for 14-bit, 65504 for 12-bit, 65535 for 16-bit/unknown. Where the margin applies, a sensor reaching its top code delivers one step above this; the 65535 cases are the container maximum and cannot be exceeded |
| `ElectronsPerADU` | **Native** `ASI_CAMERA_INFO.ElecPerADU`, read live per call — the SDK scales it by the gain register, so it tracks `Gain` (ST2) |
| `FullWellCapacity` | `NOT_IMPLEMENTED` (no native field; placeholder only if ConformU demands) |
| `ExposureMin` / `Max` / `Resolution` | From `ASIGetControlCaps(ASI_EXPOSURE)` (µs) |
| `Gain` / `GainMin` / `GainMax` | `ASI_GAIN` control; `NOT_IMPLEMENTED` if absent |
| `Offset` / `OffsetMin` / `OffsetMax` | `ASI_OFFSET`/brightness control; `NOT_IMPLEMENTED` if absent |
| `ReadoutMode` / `ReadoutModes` | The camera's download formats from `SupportedVideoFormat`, `Raw16` before `Raw8` (RM1); drives the download format and `MaxADU` |
| `SensorType` / `BayerOffsetX/Y` | Mono vs RGGB from `IsColorCam` / `BayerPattern` |
| `CoolerOn` / `CCDTemperature` / `SetCCDTemperature` / `CoolerPower` | Gated on `IsCoolerCam` |
| `CanSetCCDTemperature` / `CanGetCoolerPower` | `true` iff `IsCoolerCam` |
| `HasShutter` | `false` (ASI sensors are shutterless) |
| `CameraState` | `Idle` / `Exposing` / `Error` |
| `PercentCompleted` | From remaining-exposure µs, clamped ≤ 100 |
| `CanAbortExposure` / `CanStopExposure` | `true` / `true` (both via `ASIStopExposure`) |
| `CanPulseGuide` | `true` iff ST4 port present |
| `PulseGuide` / `IsPulseGuiding` | Asynchronous `ASIPulseGuideOn/Off` (ST4): returns immediately, `IsPulseGuiding` true until `now + duration` (PG2) |
| `StartExposure` (`Light=false`) | Accepted; captured normally (no shutter) |
| `StartExposure` / `AbortExposure` / `StopExposure` / `ImageReady` / `ImageArray` / `ImageArrayVariant` | Per *Exposure* contracts; `ImageArray` axes `[X, Y]` |

---

## Service lifecycle (`main.rs`)

Standard shape per [`service-lifecycle.md`](../skills/service-lifecycle.md):

```rust
use rusty_photon_service_lifecycle::{ServiceResult, ServiceRunner};

fn main() -> ServiceResult {
    let args = Args::parse();
    rusty_photon_service_lifecycle::init_tracing(args.log_level);

    // The default config materializes at the default path on first start. The
    // empty identity-pointer list is deliberate: ASCOM UniqueIDs are derived
    // from the camera SDK serials at enumeration (see "Device identity"), not
    // minted into config.
    let config_path = rusty_photon_config::resolve_and_init(
        "zwo-camera",
        args.config,
        &serde_json::to_value(Config::default())?,
        &[],
    )?;

    ServiceRunner::new("zwo-camera")
        .with_reload()
        .run_with_reload(|shutdown, reload| async move {
            loop {
                let bound = ServerBuilder::new()
                    .with_config_source(&config_path, CliOverrides { port: args.port })
                    .with_reload_signal(reload.clone())
                    .build()
                    .await?;           // eager SDK open + enumerate/register devices
                tokio::select! {
                    r = bound.start(shutdown.cancelled()) => return r,
                    () = reload.recv() => continue,
                }
            }
        })
}
```

`info!("Service started successfully …")` only after the bind succeeds; everything
else is `debug!` (CLAUDE.md Rule 9).

---

## Testing

Layered per [`testing.md`](../skills/testing.md). Phase E landed **45 unit tests**
and **57 BDD scenarios** (all green), plus a full **ConformU** pass; the suite
now stands at **86 unit tests** and **65 BDD scenarios**.

- **Unit** (`src/*.rs` `#[cfg(test)]`) — config parse/newtype validation, ROI/
  binning geometry math (including the %8 / %2 alignment rules), the `Camera`
  state machine (Idle/Exposing/Error, `ImageReady`, percent-completed), gain/
  offset range checks, cooling gating, Bayer-offset mapping, the format-aware
  `MaxADU` (ST3, incl. the shift, the one-step margin, and the no-shift
  depths that take none), the readout-format
  negotiation and its `to_image_array` unpacks, the gain scaling of
  `ElectronsPerADU` (ST2), and the paths the `zwo-rs` simulation can't force
  (mid-exposure SDK error E9; a model without an ST4 port PG2; an uncooled
  model K1; a camera advertising no raw format at all, RM3) — against the
  in-crate `backend.rs` mock seam over the SDK. The reconnect-during-an-exposure
  contract (E10) is unit-tested from both sides: the mock seam gates a capture
  *before* it can read its stop signal, so the disconnect → reconnect → second
  `StartExposure` interleaving is forced rather than raced, and the production
  `ZwoCameraHandle` is driven against the `zwo-rs` simulation with its camera
  closed and reopened mid-capture.
- **BDD** (`bdd-infra::ServiceHandle`, the six live camera feature files) —
  connection lifecycle (C0–C4), ROI/bin validation (R1–R3, B1–B3), exposure
  happy-path + error paths (E1–E8, incl. the graceful-stop / abort split; E9's
  mid-exposure Error transition is unit-tested), gain/offset/readout (GO1–RM1),
  cooling (K1–K4), sensor type & signal (ST1–ST3), pulse-guiding (PG1–PG2), and
  config actions, driven against the `zwo-rs` `simulation` backend.
  (FilterWheel FW1–FW3 moved to the future `zwo-filterwheel` service — ADR-014.)
- **ConformU** (`tests/conformu_integration.rs`, gated by the `conformu` feature)
  — launches the production binary with `--features simulation` and runs
  `bdd_infra::run_conformu("camera", …)`. Skipped when
  `CONFORMU_PATH` is unset. **Passes both suites** (`alpacaprotocol` +
  `conformance`) against the simulation backend: *"no errors, warnings or issues
  found"*, all members within their response-time targets. Three fixes got it
  there — the two `zwo-rs` sim fixes in rev `3c32e59` (writable `Exposure`
  control; parallel 52 MB frame fill, ≈11 s → ≈0.01 s, clearing the 10 s
  `StartExposure` timeout), plus two **driver** fixes that only became reachable
  once ConformU got past `StartExposure`: aligned reported `CameraXSize` (R4, see
  *Geometry*) and asynchronous `PulseGuide` (see *Guiding*). `zwo-camera` is now
  wired into `conformu.yml` (Phase G, 2026-06-18): the conformu jobs provision the
  ZWO SDK and run its ConformU on ubuntu/macOS/Windows.

**Real-hardware validation (2026-06-20).** Beyond the simulation backend, both
ConformU 4.3.0 suites (`alpacaprotocol` + `conformance`) pass against **real ZWO
hardware** on a Linux x86_64 dev box (SDK in `/usr/local/lib`, `99-asi.rules`
udev rule, world-RW USB node), driven against the production **non-`simulation`**
binary so the genuine FFI path — `zwo-camera → zwo-rs → libzwo-sys → libASICamera2.so`
— is exercised end to end, not just the fabricated simulator. Two physical
cameras were validated, each *"no errors, warnings or issues found"* with all
members within their response targets:

> **The `MaxADU` figures recorded below are the values the driver *reported* at
> the time, and ST3 has since corrected the formula behind them.** Neither run
> compared the reported ceiling against the pixel values actually delivered; the
> later ASI120MC-S measurement (below) shows ASI left-shifts sub-16-bit data into
> the Raw16 container, so a 12-bit camera delivers up to 65520, not 4095. The
> **ASI178MM has since been re-measured** on the bench (2026-08-05) and now
> reports **65528** — see *ASI178MM — the delivered ceiling* below, the run
> that established ST3's one-step margin. The **ASI1600MM-Cool has since been
> re-measured too** (same evening) and reports **65504**, which its hardware
> was confirmed to deliver exactly. Both cameras named here are now measured
> against their delivered pixels, closing
> [#888](https://github.com/rusty-photon/rusty-photon/issues/888).

- **ASI1600MM-Cool** (cooled, mono): `MaxADU` 4095 (12-bit), `ElectronsPerADU`
  0.00496 *(the camera was at gain 600; `ElecPerADU` is gain-scaled — see*
  `ElecPerADU` is gain-scaled *below — so this is 4.96 e⁻/ADU at gain 0)*,
  sensor 4656×3520 reported as **4608×3504** (R4 align — largest multiples of
  `lcm(8·bin)`=96 / `lcm(2·bin)`=24 for bins 1–4), gain 0–600 /
  offset 0–100, ST4 `CanPulseGuide`, both stop+abort. The cooler path was
  separately exercised live (`CoolerPower` ramped from a −10 °C target). This
  model exposes neither a serial nor a flash ID, so it used the `noserial-0`
  identity fallback (`mint_identity`) — the documented older-model path.
- **ASI178MM** (uncooled, mono): `MaxADU` 16383 (14-bit), `ElectronsPerADU`
  0.00258 *(at gain 510, i.e. 0.916 e⁻/ADU at gain 0)*,
  sensor 3096×2080 reported as **3072×2064** (R4 align), gain 0–510 /
  offset 0–600. The uncooled cooler-gating contract (K1) is confirmed on
  hardware — `CanSetCCDTemperature`/`CanGetCoolerPower` are `false` and the cooler
  getters return `NotImplemented` — while `CCDTemperature` still reads the live
  sensor value (25.6 °C), exactly the decoupled-temperature decision (K2). It
  reports a real `ASIGetSerialNumber`, exercising the canonical UniqueID path.

> One benign observation: the very first `CCDTemperature` read immediately after
> connect can return a stale `0.0 °C`, then immediately reflects the sensor
> (~15.7 °C ambient on the bench). The driver reads the value live with no caching
> (`camera.rs` `ccd_temperature`), so this is the ASI SDK's `ASI_TEMPERATURE`
> register not yet populated until its first internal measurement cycle (~1 s) —
> an SDK warm-up artifact, not a driver caching defect or a conformance failure.

**ASI120MC-S — readout-format negotiation (RM1-RM4, ST3).** A third physical
camera, measured on the Linux dev box against the real SDK while implementing
the format negotiation. It matters because it is the model family
`indi-asi` singles out as unable to do reliable 16-bit, so it is the camera
whose behaviour decides whether enumeration is a sufficient selection rule:

- **12-bit, 1280×960, colour (Bayer), USB3** (`is_usb3 = true` — the "-S"
  refresh of the USB2 original), bins `[1, 2]`.
- **It advertises `SupportedVideoFormat = [Raw8, Rgb24, Y8, Raw16]`** —
  including `Raw16`. So enumerating the array alone would *not* have excluded
  16-bit, exactly as INDI's name-based guard implies.
- **But `Raw16` works.** `ASISetROIFormat` accepted it (no
  `ASI_ERROR_INVALID_IMGTYPE`) and a frame downloaded correctly at 320×240,
  full-frame 1280×960, and 640×480 bin 2. **This contradicts the premise that
  the camera is unusable in 16-bit**, and is why INDI's blanket
  `strstr(name, "ASI120")` guard is not ported (RM4) — it would force a working
  camera to 8 bits.
- **`Raw16` is a bare left shift, not a rescale**: every pixel's low 4 bits are
  zero at bin 1, and a saturated full frame reaches exactly `4095 << 4 = 65520`
  — the measurement behind ST3's corrected `MaxADU`. **Re-confirmed 2026-08-05**
  with the deliberate-overexposure method: 65520 is reached at every gain
  across the camera's 0-100 range, so unlike the ASI178MM and ASI1600MM-Cool
  this sensor really does deliver its top ADC code. It is therefore the one
  camera that pays for ST3's margin — measured at **3 pixels in 1 228 800**
  called saturated one LSB early, against 6 095 correctly flagged (see ST3).
- **End-to-end through the driver** (production non-`simulation` binary, real
  camera, over Alpaca): `ReadoutModes` reports `["Raw16", "Raw8"]`,
  `ReadoutMode` defaults to 0, and switching mode changes both the delivered
  frame and `MaxADU` consistently — mode 0 gave `MaxADU` 65520 with a 64×48
  frame ranging 16-17872, mode 1 gives `MaxADU` 255 with the same geometry
  ranging 0-48. **The 8-bit download path is hardware-proven**, not just
  simulated. *(The 65520 is what this run recorded; ST3 has since added the
  one-step margin, so this camera now reports 65504 — one ADC LSB below the
  ceiling it was measured reaching. See the ASI178MM run below for why.)*

`crates/zwo-rs/examples/probe_formats.rs` is the probe that produced these
numbers; re-run it against any new ZWO model rather than assuming this one
generalises.

**ASI178MM — the delivered ceiling (2026-08-05).** The 14-bit half of the same
question, re-measured on the bench at commit `269a4cc3` against the real SDK,
because the ASI120MC-S alone could not distinguish a `16 - BitDepth` left shift
from a model that delivers 14-bit data unshifted (which would have needed
`MaxADU` 16383 after all). Full record:
[docs/validation/2026-08-05-zwo-camera-asi178mm-maxadu/](../validation/2026-08-05-zwo-camera-asi178mm-maxadu/README.md).

- **The shift model holds.** The camera advertises `[Raw8, Raw16]`, and its
  `Raw16` pixels carry **two always-zero low bits** at bin 1 — a left shift by
  `16 - 14`, exactly as on the 12-bit camera. The 16383 this doc recorded in
  2026-06-20 was wrong by a factor of four.
- **But the sensor clips one ADC count short.** Comprehensively blown-out
  frames top out at exactly `16382 << 2 = 65528`, never 65532 — measured at
  gains 0/100/300/510, bins 1-3, and exposures 1 s/5 s/15 s, with ~98 000
  pixels piled on 65528 and nothing above it. **This is the measurement behind
  ST3's one-step margin.** Before it, the driver reported the shifted full
  scale and a saturated full frame over Alpaca gave `pixels >= MaxADU` = **0**
  while 13 655 pixels sat at the sensor's ceiling; with the margin the same
  frame reports **6 709** saturated pixels.
- **Binning changes the packing but not the ceiling.** At bin ≥ 2 the SDK
  combines neighbouring counts and the low bits populate, so the shift
  signature is only visible at bin 1. The ceiling stays 65528, so one published
  `MaxADU` still describes every bin.
- **ConformU 4.4.0 clean on both suites** against the negotiated list —
  `ReadoutModes Read OK Raw16` / `OK Raw8` — with zero errors, issues,
  configuration alerts or timing issues, both before the margin
  (`MaxADU OK 65532`) and after it (`MaxADU OK 65528`).

`crates/zwo-rs/examples/probe_ceiling.rs` is the probe behind this one: it
deliberately overexposes and prints the histogram tail, which is what separates
a real clip from the brightest thing in the room.

**ASI1600MM-Cool — the delivered ceiling (2026-08-05).** The 12-bit half of
#888, measured the same evening. Full record:
[docs/validation/2026-08-05-zwo-camera-asi1600mm-cool-maxadu/](../validation/2026-08-05-zwo-camera-asi1600mm-cool-maxadu/README.md).

- **It clips one ADC count short as well.** Driven to complete saturation
  (gain 600, 15 s), **every** pixel of the frame — all 16 389 120 at bin 1,
  and the whole frame at bins 2-4 — sits at exactly `4094 << 4` = **65504**,
  with nothing above. The same ceiling shows as a clip at lower gains
  (`65504×28` at gain 100), so it is fixed, not an over-driven-gain artifact.
  End-to-end through the driver a saturated frame reports `pixels >= MaxADU` =
  **16 146 432**, i.e. all of them.
- **The margin is exact here too.** `((2^12) - 2) << 4 = 65504` is the measured
  value, not an approximation — as `((2^14) - 2) << 2 = 65528` was on the
  ASI178MM. Note this is a *different* answer from the ASI120MC-S, which shares
  its 12-bit depth but does reach `4095 << 4`: the shortfall is a property of
  the sensor, not of the bit depth.
- **Binning walks the shift down** rather than destroying it: 4 zero bits at
  bin 1, 2 at bin 2, none at bins 3-4 — consistent with the SDK averaging the
  binned pixels. The ceiling is unchanged at every bin.
- **Cooling (K1-K4) re-confirmed with the TEC powered**: `CoolerPower` ramps
  0 → 24 % while the sensor falls 17.7 °C → 6.2 °C in 120 s, and returns to
  0 % when switched off. `CoolerOn` reads `false` after every connect — tenet
  3 holding on hardware.
- **ConformU 4.4.0 clean on both suites** (0/0/0/0, 87 timed members) with
  `MaxADU OK 65504`, run with the cooler powered.

**Three cameras on one service (2026-08-05).** With the ASI1600MM-Cool,
ASI178MM and ASI120MC-S all attached, the enumeration contracts were exercised
on hardware for the first time with more than one body — the BDD suite
presents a single simulated camera, so C0/C4 had only ever been simulated.

- **C0** — all three registered with distinct UniqueIDs, exercising *both*
  identity paths side by side: real `ASIGetSerialNumber` on the ASI178MM and
  ASI120MC-S, and the `noserial-0` fallback on the ASI1600MM-Cool, which
  exposes neither a serial nor a flash id.
- **C4** — connect is per-device (`[T,F,F] → [T,T,F] → [T,T,T]`), and a
  2 s exposure on one camera left the other two at `CameraState` `Idle` with
  `ImageReady` false.
- **RM1/RM4 against a camera that really offers the excluded formats** — the
  ASI120MC-S advertises `[Raw8, Rgb24, Y8, Raw16]` and the driver publishes
  `["Raw16", "Raw8"]`, so the `Rgb24`/`Y8` exclusion is confirmed on hardware
  rather than only in the simulator.
- **K1 both ways at once** — the cooled body reports
  `CanSetCCDTemperature`/`CanGetCoolerPower` `true` while the two uncooled ones
  return `NOT_IMPLEMENTED` for the cooler getters, in the same service.
- **Tenet 3** — `CoolerOn` read `false` after every connect, on every run.

**The shared camera-core crate on hardware (2026-08-07).** The same three
bodies, re-run at `7e12a9b3` on ConformU 4.5.0 after the ROI rule set, the
Bayer offsets and the single-plane `ImageArray` unpack moved out of the three
camera drivers into
[`rusty-photon-camera-core`](../../crates/rusty-photon-camera-core/). Both
suites clean on all three; full record in
[docs/validation](../validation/2026-08-07-zwo-camera-three-cameras-linux/README.md).
What it established that the simulator could not:

- **The Bayer chain end to end, on a physical mosaic.** The ASI120MC-S reports
  `BayerOffsetX/Y = (1, 0)` — GRBG. That is `ASI_BAYER_GR` travelling through
  `zwo-rs`'s `BayerPattern::Gr`, this driver's map onto camera-core's `Grbg`,
  and the shared `offsets()` rule. `Gr`/`Gb` are the pair whose offsets are
  transposes of each other, so it is the case a mis-mapping would show up in.
- **The shared `GeometryError` text and its ASCOM code reach the client
  unchanged.** ConformU's sub-frame rejection tests record the crate's own
  message arriving via `From<GeometryError> for ASCOMError` — *"NumX must be a
  multiple of 8 and NumY a multiple of 2"*, *"StartX + NumX exceeds
  CameraXSize / BinX"* — so collapsing three `map_err` call sites into one
  conversion altered neither the wording nor `INVALID_VALUE`.
- **The shared unpack at both depths across three sensors**, over the
  negotiated `Raw16`/`Raw8` modes, all within ConformU's response targets.

**The reconnect contract on hardware (2026-08-23).** The same three bodies,
re-run at `2bc56edc` on ConformU 4.5.0 after the E10 fix — the per-capture stop
cell and the `open_epoch` gate. Both suites clean on all three; full record in
[docs/validation](../validation/2026-08-23-zwo-camera-three-cameras-reconnect/README.md).
Every reported figure (geometry, `MaxADU`, `ElectronsPerADU`, gain/offset
ranges, cooling gating) matches the 2026-08-07 run exactly, which is the point:
the change is internal to how a capture is cancelled, and nothing a client can
observe moved. What the run adds beyond that:

- **The exposure paths this touched, on the real SDK.** E7 abort discards and
  the device reaches `Idle` (0.10 s on the ASI178MM, 0.36 s on the
  ASI1600MM-Cool, 1.43 s on the ASI120MC-S, out of a 20 s exposure); a new
  exposure is accepted immediately afterwards; E8's graceful stop publishes the
  partial frame in the same order of time; C3's disconnect cancels and surfaces
  nothing on reconnect. That is the per-capture cell draining a capture through
  three different vendor bodies, not the simulator's approximation of it.
- **E10 swept across the whole capture.** Twelve disconnect points per camera —
  through integration and past the exposure's end into the readout/download
  phase of a 12.7 MB full frame — each followed by a reconnect and a
  *differently sized* second exposure, which had to complete on time with its
  own geometry. 12/12 on each body. The size difference is what makes it a
  test: a frame produced by the superseded capture carries the first exposure's
  dimensions.
- **A severity calibration, from a failed reproduction.** The race itself does
  not fire through the Alpaca API on a healthy box: pre-fix `main`, built in a
  throwaway worktree and driven by the same harness against the same camera,
  stayed clean over 12 sweep trials and 80 soak iterations, including with the
  service pinned to a single core crowded by 64 spinners. The window needs the
  reconnect *and* the next `StartExposure` — ~300 ms of USB/SDK work — to land
  before the superseded capture's next 20 ms poll, and uniform starvation
  stretches both sides equally. It opens when that one capture thread is
  delayed past the reconnect, i.e. under the blocking-pool overshoot documented
  in *Concurrency* above. So the defect is real but needs a badly starved
  capture thread, and the deterministic proof stays in the unit tests (each
  verified to fail against the pre-fix shape) rather than on hardware.

**Recorded validation runs (2026-07-27).** The 2026-06-20 runs above predate
the [hardware validation record trail](../validation/README.md), so their
ConformU output was not preserved. Recorded re-runs against the same physical
ASI1600MM-Cool exist for both platforms, both fully clean:
the [Linux record](../validation/2026-07-27-zwo-camera-asi1600mm-cool-linux/README.md)
(ConformU 4.3.0) and the
[Windows record](../validation/2026-07-27-zwo-camera-asi1600mm-cool-windows/README.md)
(ConformU 4.4.0) — the **first Windows real-hardware validation** of this
service, in a Windows 11 KVM guest with the camera on QEMU USB passthrough.
The Windows facts: ZWO's native camera driver (v3.28, `asicamusb3.inf`) is
**required** — the camera carries no MS OS descriptors, so WinUSB never
auto-binds (same as the SVBONY SV605CC) — but unlike SVBony's it is a
captcha-free direct CDN download; the SDK is the `ASI_Windows_SDK_V1.41` zip
from the same rolling developer-CDN URL `install-zwo-sdk` uses
(`ZWO_SDK_LIB_DIR` for the link, `ASICamera2.dll` beside the exe); and
bindgen means the build host needs LLVM/libclang (`LIBCLANG_PATH`), which CI
runners ship but a fresh Windows box does not. UniqueID minting is identical
on both platforms. One cross-platform SDK discrepancy surfaced
([#741](https://github.com/rusty-photon/rusty-photon/issues/741)):
`ASI_CAMERA_INFO.ElecPerADU` for the same camera reads **4.96** through the
Windows v1.41 DLL but **0.00496** through the Linux v1.41 blob — an exact
1000× split, with the Windows value the physically plausible one (the June
narrative values above, read through the Linux blob, carry the same 1000×
scaling); the driver reports the SDK value verbatim on both platforms (ST2).

**Concurrent multi-camera validation (2026-06-21).** A single service instance
enumerates every connected ASI camera on the one port, so concurrency *across*
devices is a first-class case. With **three** cameras attached at once —
**ASI178MM** (`camera/0`), **ASI120MC-S** (`camera/1`, a colour USB2 planetary
model) and **ASI1600MM-Cool** (`camera/2`) — a full ConformU run
(`alpacaprotocol` + `conformance`) was fired at all three **simultaneously**
(three independent processes, isolated `$HOME`). **All three passed**, each *"no
errors, warnings or issues found"* and — critically — *"all members returned
within their target response times"* **under 3× concurrent load**, the very
scenario the deadline-based integration wait (see *Concurrency*) was hardened
for. The three suites overlapped throughout a ~46 s window with no driver-level
errors, mid-exposure `Error` transitions, late-capture invalidations, or
lock-contention symptoms; the service enumerated all three (`cameras=3`), shut
down cleanly, and left every camera released on USB. New coverage from this run:
the **ASI120MC-S colour path** (`SensorType RGGB`, `BayerOffsetX/Y`) and
per-device independence (contract C4) under genuine concurrency. One operational
note surfaced: the serial-less ASI1600 came up as `noserial-2` here (it is
`noserial-0` when attached alone), so the position-based `mint_identity` UniqueID
is **enumeration-order-dependent** in multi-camera setups — cameras that report a
real SDK serial (ASI178MM, ASI120MC-S) are order-independent.

> **CI caveat (critical):** the `simulation` feature removes the *camera*
> requirement, **not the SDK**. All build/test/ConformU jobs for this package
> still link `ASICamera2` (this service's device feature — ADR-014; the shared
> Bazel `zwo-rs` targets link the full union), so CI must install the SDK first
> (see *Gating plan*). Only `cargo check`/clippy jobs (which don't invoke the
> linker) can skip the SDK.

### `ElecPerADU` is gain-scaled — reading it live (ST2)

`ASI_CAMERA_INFO.ElecPerADU` is **not** a static per-model constant. The SDK
stores the model's gain-0 figure in a per-model table and divides it by the
camera's current gain before handing it over, so the field tracks the gain
register. Measured across three bodies:

| Camera | Gain range | `ElecPerADU` at gain 0 → max | Scaling law |
|---|---|---|---|
| ASI1600MM-Cool | 0–600 | 4.96 → 0.00496 | `10^(gain/200)` (0.1 dB units) — 60.000000 dB |
| ASI178MM | 0–510 | 0.916 → 0.0025816 | `10^(gain/200)` — 51.000000 dB |
| ASI120MC-S | 0–100 | 3.52 → 0.055 | **neither** — ÷3.125, ÷9, ÷64 at gain 25/50/100 |

The modern bodies follow ASI's 0.1 dB gain convention exactly. The legacy
ASI120MC-S does not: its gain scale is 0–100 and the mapping is something else
entirely (the 0.1 dB law would predict ÷1.33, ÷1.78, ÷3.16 at those gains).

**Checked at every gain each camera advertises** (601 / 511 / 101 values, read
through the driver over Alpaca):

| Camera | vs `10^(gain/200)` | monotonic | distinct values |
|---|---|---|---|
| ASI1600MM-Cool | worst error **0.000 %** across 601 gains | yes | — |
| ASI178MM | worst error **0.000 %** across 511 gains | yes | — |
| ASI120MC-S | worst error **95.06 %** (at gain 100) | yes | 101 of 101 |

The two modern bodies match the 0.1 dB law to the float's precision at *every*
gain, not merely at sampled points. The ASI120MC-S misses it by 95 % at the top
of its range — exactly the ÷64-versus-÷3.16 gap above — and its divisor ladder
walks 1, 1.3125, 1.625, 1.9375, 2.5, 3.125 … 32, 40, 48, 56, 64 in segments
that fit no single expression. It returns a **distinct value at all 101 gains**,
so the property really does track every step of the register.

**That is the case for reading the value rather than computing it.** Any
driver-side formula would have to be right for every model ZWO has ever shipped,
and the ASI120MC-S alone proves there is no single formula. Reading live is
law-agnostic — the driver never needs to know the mapping.

The gain-0 values are visible in the blob as float32 constants (`4.96f` for the
ASI1600, `0.916f` for the ASI178), and each reconciles with its sensor's physics
— 0.916 e⁻/ADU against the IMX178's ≈15 ke⁻ over a 14-bit ADC ≈ 0.92, and
3.52 e⁻/ADU against the ASI120's ≈14 ke⁻ over 12 bits.

**This is what the driver got wrong.** `ElectronsPerADU` used to be served from
the `CameraInfo` snapshot captured at enumeration, so it reported the value for
whatever gain the camera held at service startup and never moved when a client
changed `Gain`. A client that sets gain and then reads `ElectronsPerADU` — the
normal sequence for SNR or exposure math — got a stale number. ST2 now reads it
live, through `Camera::electrons_per_adu` (`ASIGetCameraPropertyByID`, an
open-camera call, rather than the enumeration-index `ASIGetCameraProperty`).

**It also explains a stale piece of validation folklore.** The 2026-06-20 and
2026-07-27 hardware runs recorded `ElectronsPerADU` figures — 0.00496 for the
ASI1600MM-Cool, 0.00258 for the ASI178MM — that look absurd next to the
Windows-side 4.96 for the same ASI1600, and were briefly read as a 1000×
Linux/Windows SDK split. They are neither absurd nor a platform split: those
cameras were simply sitting at **maximum gain** (600 and 510) during the Linux
runs, while the Windows readings were taken at gain 0 (ASI1600) and gain 210
(ASI178). Driven back to gain 0 on Linux, the ASI1600 reports
`4.960000038146973` — **bit-identical** to the Windows figure. The apparent
1000× and √1000× ratios are exactly 60.000000 dB and 30.000000 dB, gain deltas
to eight significant figures rather than decimal scaling. The figures in the
validation records above are annotated with the gain they were taken at.

There is consequently **no plausibility check on this value, and there must not
be one**: at high gain a genuinely tiny `ElectronsPerADU` is correct, and
`MaxADU × ElectronsPerADU` is *supposed* to shrink — that is what gain means.

Reproducer (needs the SDK + a camera; reads properties and writes the gain
control, no exposure):

```c
ASI_CAMERA_INFO info;
ASIGetCameraProperty(&info, 0);
ASIOpenCamera(info.CameraID); ASIInitCamera(info.CameraID);
for (long g = 0; g <= 510; g += 255) {
    ASISetControlValue(info.CameraID, ASI_GAIN, g, ASI_FALSE);
    ASIGetCameraPropertyByID(info.CameraID, &info);
    printf("gain %3ld -> ElecPerADU=%.17g\n", g, (double)info.ElecPerADU);
}
```

---

## Delivery phasing

This service is built in tracks to isolate the genuinely novel risk (the FFI
crate + native system dependency) from the mechanical-but-large risk (the device
driver itself). The FFI crate is the long pole (~40–50% of effort); once
`simulation` works, the driver builds entirely against it, leaning on the
`sky-survey-camera` + `qhy-camera` scaffolding. Tracks A–G mirror
[`docs/plans/zwo-driver.md`](../plans/zwo-driver.md):

- **Phase A — `libzwo-sys`** *(skeleton stood up)*. `bindgen` over `ASICamera2.h`
  + `EFW_filter.h` + `EAF_focuser.h`; `build.rs` unconditional system-link
  (per-device features since ADR-014). Green
  `check` + `test` on Linux x86_64, built + tested locally on aarch64.
  *Remaining:* confirm green link on Pi 5 aarch64 CI + macOS arm64.
- **Phase B — `zwo-rs`** *(skeleton stood up)*. Safe `Sdk`/`Error` surface +
  `simulation`-feature stub. *Remaining:* real safe handles/enums + error mapping
  + the `simulation` backend (camera frames + EFW position/moving); publish 0.1.0.
- **Phase C — Track A.** Bare `zwo-camera` serving an empty/sim Camera on
  `:11122`; prove build/link, CI SDK provisioning (Cargo *and* Bazel
  workflows), Pi 5 aarch64, Bazel as a first-class `//...` target (no `manual`
  gating), repin-twice — *before* device-trait work.
- **Phase D — design doc + ADR + workspace row + BDD feature files** *(this
  document, [ADR-008](../decisions/008-zwo-camera-native-sdk-ffi.md), the
  `docs/workspace.md` row, and the `@wip` feature files)*.
- **Phase E — Track B full Camera** *(landed)*. `Device + Camera` over `zwo-rs`
  (ROI/bin, gain/offset, cooling, readout, exposure state machine, abort +
  graceful stop, PulseGuide, sensor type), config-actions, serial identity,
  `spawn_blocking` bridge (camera lock released during integration), `backend.rs`
  mock seam. 45 unit tests + 57 BDD scenarios green (ConformU passes); the six camera feature files
  are live.
- **Phase F — EFW `FilterWheel`: re-scoped to a future separate
  `zwo-filterwheel` service** (ADR-014, 2026-07-10) — see
  [`docs/plans/zwo-driver.md`](../plans/zwo-driver.md) Phase F. Not part of
  this service; the `@wip` `filter_wheel.feature` and the `filterwheel.enabled`
  config toggle were removed with the re-scope.
- **Phase G — test + gate + consumer.** BDD landed (Phase E). **ConformU passes
  both suites** against the simulation backend (verified locally) after the
  `zwo-rs` rev `3c32e59` sim fixes plus the aligned-`CameraXSize` (R4) and
  asynchronous-`PulseGuide` driver fixes; the `conformu_integration.rs` harness
  is in place. Remaining: wire `zwo-camera` into `conformu.yml`; `rp`
  `CameraConfig` consumer; optional hermetic Bazel `crate.annotation` (SDK as a
  `cc_import` dep, dropping the imperative install) — cleanup, not blocking.

---

## Future Work

- **Video mode** (`ASIStartVideoCapture`) as a high-FPS guiding/planetary path.
- **CAA rotator** (`CAA_API.h`) if a ZWO rotator is ever in scope.
- **Vendoring the SDK** into `libzwo-sys` (MIT permits) to drop external
  provisioning entirely.
- **Backport** the SDK-free-simulation / feature-gated-link improvement to
  `qhyccd-rs` so `qhy-camera`'s default build can also be pure-Rust.
- Per-serial connect-time tuning; `FullWellCapacity`; TLS / Basic Auth via
  `rusty-photon-tls` / `rp-auth`.

## Packaging

Packaged as `rusty-photon-zwo-camera` (`.deb`/`.rpm`) per
[ADR-012](../decisions/012-service-packaging-architecture.md) /
[ADR-013](../decisions/013-native-sdk-payload-policy.md) and
[`docs/plans/service-packaging.md`](../plans/service-packaging.md):
binary at `/usr/bin/rusty-photon-zwo-camera`, hardened
`rusty-photon-zwo-camera.service` (camera class: `AF_NETLINK`, no
`PrivateDevices`/`MemoryDenyWriteExecute`, no supplementary groups), and
a udev rule `90-rusty-photon-zwo.rules` assigning enumerated ZWO devices
(VID `03c3`) to the `rusty-photon` service group plus the usbfs memory
bump.

Unlike qhy-camera, the native SDK ships **inside the package**: ZWO's
blobs are MIT-licensed, so `libASICamera2.so` — **exactly the one SDK this
binary links** (zwo-rs `camera` feature, ADR-014) — is downloaded at
package-build time by `scripts/build-packages.sh` from the same pinned
indi-3rdparty commit `.github/actions/install-zwo-sdk` uses, and installed
at `/usr/lib/rusty-photon/`, with the SDK license at
`/usr/share/doc/rusty-photon-zwo-camera/ZWO-SDK-LICENSE.txt`. Because each
zwo package owns only its own blob, this package co-installs cleanly with
`rusty-photon-zwo-focuser` (which ships `libEAFFocuser.so`). The blob
carries no SONAME, so the packaged binary locates it via a RUNPATH
(`-Wl,-rpath,/usr/lib/rusty-photon`) injected by the build script — no
`ldconfig`, no `/usr/local` spill, no external SDK install step for the
operator. ZWO cameras keep their firmware in onboard flash, so there is
no firmware-install helper and no cold-plug upload path.

## References

- Decision record: [`docs/plans/zwo-driver.md`](../plans/zwo-driver.md) ·
  [ADR-008](../decisions/008-zwo-camera-native-sdk-ffi.md)
- FFI crates (this repo's author): [`zwo-rs`](https://github.com/ivonnyssen/zwo-rs)
  + `libzwo-sys` (siblings to `qhyccd-rs` / `libqhyccd-sys`)
- Same-vendor-class precedent: [`qhy-camera.md`](qhy-camera.md) ·
  [`qhy-focuser.md`](qhy-focuser.md)
- Camera scaffolding template: [`sky-survey-camera.md`](sky-survey-camera.md)
- [`config-actions.md`](config-actions.md) ·
  [`service-lifecycle.md`](../skills/service-lifecycle.md) ·
  [`development-workflow.md`](../skills/development-workflow.md) ·
  [`testing.md`](../skills/testing.md)
- Behavioural references (read-only, clean-room): INDI `indi-asi` (LGPL-2.1+ /
  GPL-2.0+ per file), [`python-zwoasi`](https://github.com/stevemarple/python-zwoasi),
  INDIGO `indigo_drivers/{ccd,wheel,focuser}_asi`
- ASI/EFW SDK (headers + per-arch binaries, MIT): INDI `indi-3rdparty/libasi`
- [ADR-001 Amendment A](../decisions/001-fits-file-support.md) — the pure-Rust /
  no-system-dep posture this service is the second exception to (after
  `qhy-camera`)

# zwo-camera: the status banner, 2026-06 to 2026-07

A record, moved whole from `docs/services/zwo-camera.md` on 2026-11-13: the
status banner that stood at the head of the design document — the full
Camera phase and its test counts, the fixes that took ConformU to a clean
pass, the first real-hardware passes (2026-06-20) and the recorded re-runs
(2026-07-27), the EFW's move to a separate service, and the CI provisioning
with the validation then still open. It stood at the top of
[zwo-camera.md](../../services/zwo-camera.md#zwo-camera-service-design),
above § Overview; that document states what the service does today. The
sections the banner names in italics are that document's, except *Open
questions*, which it no longer has, and *Delivery phasing*, whose phases are
[the plan's](../zwo-driver.md).

> **Status:** **Phase E (Track B full Camera) landed.** The `services/zwo-camera`
> crate now implements the full ASCOM `Device` + `Camera` surface over the
> `zwo-rs` SDK seam (`backend.rs` → production `ZwoCameraHandle` + a unit-test
> mock): connection lifecycle, sensor geometry, symmetric binning, ROI with the
> ASI `%8`/`%2` alignment rules, gain/offset, cooling, readout modes, ST4
> pulse-guiding, and the snap-mode exposure state machine (start, abort
> *discards* / graceful stop *preserves*, `ImageArray`, `CameraState`,
> `PercentCompleted`, mid-exposure `Error`), plus serial-derived identity and the
> setup pages. The blocking SDK chain runs on
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
> [ADR-014](../../decisions/014-zwo-per-device-services-and-link-features.md)
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
> [docs/validation/](../../validation/README.md) on both Linux **and Windows 11**
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

# svbony-camera: how the driver was delivered, 2026-07 to 2026-10

A record, moved whole from `docs/services/svbony-camera.md` on
2026-11-13: the status banners that stood at the head of the design
document — the phases the driver was built in (E, F and G), the two
real-hardware validation passes of 2026-07-26, and the follow-ups that
changed the packaging, the readout modes, the connect handshake, the
gain and offset, a departed camera's `Connected` and the device claims.
It stood at the top of
[svbony-camera.md](../../services/svbony-camera.md#svbony-camera-service-design),
above § Overview; that document states what the service does today, and
the sections the banners name in quotes are its own — "This phase's
link-gating shortcut" is its § Bazel links the simulation only — except
"Real-hardware validation", whose dated account is
[the record of the validation passes](svbony-camera-hardware-validation-2026.md),
and "Delivery phasing", which is the
[archived plan's](svbony-camera.md#delivery-phasing).

> **Real-hardware validation PASSED (2026-07-26, physical SV605CC).** The
> camera arrived and the full validation pass ran on a Linux dev machine
> against the real SDK (staged from the pinned indi-3rdparty blob):
> enumeration/identity from the hardware `CameraSN`, connect handshake,
> gain/offset/cooling round-trips, the soft-trigger exposure state machine
> at multiple ROIs/bins including the full 3008×3008 frame (~18 MB Raw16,
> needs the udev rule's `usbfs_memory_mb` bump), abort + prompt recovery,
> and ASCOM **ConformU 4.3.0: both the `alpacaprotocol` and full
> `conformance` suites pass with zero errors/issues** against the physical
> camera. Four hardware-driven changes landed with the validation (all in
> the same PR): (1) production `build()` now registers real enumerated
> cameras — the long-standing simulation-only registration boundary is
> gone; (2) `MaxADU` is now 65535 (the SDK rescales the 14-bit ADC to full
> 16-bit Raw16 scale — saturated pixels read 65535, not 16383); (3)
> `CameraXSize`/`CameraYSize` now report the extent aligned down so every
> binned full frame is a valid ROI (2976×3000 for the SV605CC), adopting
> `zwo-camera`'s R4 after real ConformU failed at bin 3 against the raw
> extent, exactly as Phase E's "revisit once ConformU coverage exists"
> anticipated; (4) `AbortExposure` now drains within ~one 250 ms poll slice
> (a cancel flag checked between `SVBGetVideoData` slices + a
> stop/re-arm), replacing the run-to-deadline drain that left the device
> reporting `Idle` while rejecting new exposures for the rest of the
> aborted exposure's deadline. Every open punch-list item below is
> resolved — see "Real-hardware validation" for the itemized results.
>
> **Windows validation PASSED (2026-07-26, same physical SV605CC):** the
> real-SDK Windows build (manually-staged SDK, see "Windows" below) passes
> ConformU 4.4.0 `alpacaprotocol` + `conformance` cleanly on Windows 11
> with identical `UniqueID` minting — evidence in
> [docs/validation/2026-07-26-svbony-camera-sv605cc-windows/](../../validation/2026-07-26-svbony-camera-sv605cc-windows/README.md),
> operator steps in
> [docs/svbony-camera-windows-install.md](../../svbony-camera-windows-install.md).
> CI provisioning ([#720](https://github.com/rusty-photon/rusty-photon/issues/720)
> Part 2) remains open.
>
> **Device claims ([device-claims plan](../../plans/archive/device-claims-and-phd2-camera.md)
> C2): an optional `usb_devices` list pins each USB port to an Alpaca device
> number.** A camera on a listed port is served at that entry's number every
> time, whatever order the SDK enumerates in; a camera on an unlisted port is
> never registered, so nothing opens it; and a listed number whose camera
> cannot be served is held by a **placeholder** that refuses to connect with
> one fixed error code and says why. Placement is a passive join of the host
> USB scan with the SDK's enumeration — nothing is opened to decide it — and
> a failed scan is retried in the background until it succeeds, when the
> service reloads itself. `svbony-camera doctor --devices` lists every SVBony
> camera on the bus by port and prints the list to paste. With no list —
> the permanent default — the service serves every camera the same scan
> places, numbered in port order, and refuses the rest (plan C5). See
> "Configuration" and "Device claims (`usb_devices`)" (U1-U11).
>
> **Follow-up landed (issue #1412): a camera that has left the bus reads
> disconnected.** `Connected` was the driver's own flag, so a camera whose
> power or cable was cut went on answering `Connected = true` and every
> cache-served member — `CanSetCCDTemperature` still `true` among them — and
> rp's reconnect supervisor, which takes `Connected == true` as healthy, never
> re-established it. The SDK does not say a camera has gone (measured on
> pier1's SV605CC: a departed camera's calls keep answering), so a call on the
> camera that fails asks, by rescanning the bus — a departed camera's next
> exposure fails at its read deadline, and that is the failure that usually
> asks. Only a failure asks, so an idle departure is noticed at the next
> exposure. A camera the rescan no longer finds reads `Connected = false`,
> every member answers `NOT_CONNECTED`, and a client's `Connected = false` or
> `true` releases the lost session, the latter then connecting afresh — to a
> camera that has come back too, since an open now rescans first. Connects,
> disconnects and releases are serialized per device, so no reconnect
> overtakes a handshake still running (C7). Validated end to end on pier1's
> SV605CC, a hand-pulled USB cable included (2026-10-07); Windows is still
> owed. See "Enumeration & connection lifecycle" (C6).
>
> **Follow-up landed (issue #1336): `Gain` and `Offset` are cached at the
> setter and armed by `StartExposure`.** The setters used to write
> `SVB_GAIN`/`SVB_BLACK_LEVEL` to the camera at once, with nothing keeping a
> live exposure out of their way: a capture gives the SDK mutex up between
> its 250 ms `SVBGetVideoData` slices, so a gain or offset set mid-exposure
> reached the camera under the frame it was integrating. Now a set is
> checked against the advertised range and stored, and nothing reaches the
> camera until the next `StartExposure` arms it — on every exposure, changed
> or not, after that exposure's own `SVB_EXPOSURE` write (GO2/GO5).
> `Gain`/`Offset` report the value the next exposure arms, seeded at connect
> by *reading* what the camera holds (GO1) — the bargain `BinX` and `NumX`
> already make. A set made while an exposure is in flight is taken for the
> next frame, and the frame in flight keeps the values it was armed with;
> `ReadoutMode` is now the one frame setting still refused mid-exposure
> (RM1). A value the SDK refuses at arm time fails that exposure (E9). See
> "Gain / offset / readout" (GO1-GO5).
>
> **Follow-up landed (issue #891): the connect handshake now mirrors
> `indi_svbony_ccd`'s post-open sequence — `SVBRestoreDefaultParam`,
> `SVBSetAutoSaveParam(false)`, then a manual `SVB_EXPOSURE` write — so
> `Gain` is settable from the moment `Connected` turns true.** The SDK
> refuses `SVBSetControlValue(SVB_GAIN)` (as `SVB_ERROR_GENERAL_ERROR`,
> its catch-all for every internal failure) while the camera's
> **auto-exposure state is on**, and that state is on after
> `SVBOpenCamera`; the only SDK path that turns it off is a manual
> (`bAuto = false`) `SVB_EXPOSURE` write, which this driver used to issue
> only inside the per-exposure capture path — hence "gain cannot be set
> until the first exposure". The working-directory dependency the issue
> was filed for is a *proxy* for that state: with auto-save on (the SDK
> default) the SDK dumps the whole parameter block, auto-exposure state
> included, to `<model>_Cfg_SAVE.bin` in the process cwd at close and
> reloads it at the next open, so a writable cwd merely carried the last
> session's "auto-exposure off" forward. Auto-save is now disabled at
> connect, defaults are restored explicitly, and the simulation reproduces
> the SDK's gate. (Since issue #1336 a gain set never reaches the SDK at the
> setter — the next exposure arms it, after its own exposure write — so the
> gate now constrains the arm's order rather than the setter, and the tests
> that pin it are arm-time ones; see GO5.) The mechanism was established
> from the SDK binary and
> `indi_svbony_ccd`'s `Connect()` and **confirmed on the physical SV605CC
> on the rig on 2026-08-16**: the packaged binary started from a working
> directory holding no `U3SM900C-AST_Cfg_SAVE.bin` accepts `PUT Gain`
> straight after connect (the previously deployed binary, same procedure,
> still returned the *general error*), and ConformU `alpacaprotocol` +
> `conformance` are clean from that pristine directory with zero
> informational `PUT Gain` items — see "Real-hardware validation → Field
> rig — connect handshake C1a (2026-08-16)". See also "Enumeration &
> connection lifecycle" (C1a), "Gain / offset / readout" (GO5) and
> "Working directory (SDK-persisted camera config)" below.
>
> **Follow-up landed (issue #882): the download format is negotiated from
> `SupportedVideoFormat`, and `ReadoutModes` is that list.** The driver
> used to set `Raw16` unconditionally on every exposure while
> `svbony-rs` read the camera's supported formats and nobody consulted
> them — a camera without `Raw16` would have failed at configuration on
> every exposure. `ReadoutModes` now *is* the negotiated list: at connect
> the driver intersects `SVB_CAMERA_PROPERTY.SupportedVideoFormat` with
> the formats it can deliver (`Raw16`, then `Raw8`) and publishes the
> survivors as the ASCOM readout-mode list, defaulting to index 0 (the
> highest precision the camera offers). The selection drives the
> per-exposure `SVBSetOutputImageType`, the download buffer size, the
> `ImageArray` unpack, and `MaxADU` (65535 for `Raw16`, 255 for `Raw8`).
> A camera offering neither raw format fails connect with a logged
> reason rather than downloading a debayered frame. **This replaces the
> old cosmetic `["SoftTrigger", "FreeRunning"]` readout list** — a
> breaking change to a published list, taken because the acquisition
> mode it named is not operator-selectable (it follows `IsTriggerCam`),
> whereas the download format is exactly what ASCOM's `ReadoutModes` is
> for. See "Gain / offset / readout" (RM1-RM4) for the contract and why
> `RGB24`/`RGB32` and `Y8`-`Y16` are deliberately excluded. **Validated on
> the physical SV605CC on the field rig 2026-08-05**, including a full
> 2976 × 3000 frame downloaded in `Raw8` at exactly `w × h` bytes with
> `MaxADU` 255, and ConformU 4.4.0 clean on both suites against the new
> readout list — see "Field rig — readout formats (2026-08-05)" below.
>
> **Follow-up landed (issue #679, 2026-07-22): `scripts/build-packages.sh`
> now has the `needs_svbony` SDK-staging leg this Status section's Phase G
> entry (below) originally deferred.** `nightly-packages` was failing on
> the `linux / arm64`, `linux / amd64`, and `macos / arm64 tarballs` legs
> with `unable to find library -lSVBCameraSDK`: the script discovered
> `svbony-camera` via its `pkg/` directory (like every other packaged
> service) but staged no SDK for it to link against. Fixed by staging the
> pinned indi-3rdparty blob into `SVBONY_SDK_LIB_DIR` for the link only —
> unlike QHY/ZWO, nothing is copied into `services/svbony-camera/pkg/lib/`
> or bundled in the package (ADR-018: no license grant at all), so the
> RUNPATH `build-packages.sh` already bakes in for `zwo-camera` is what
> lets the operator-installed copy (`rusty-photon-svbony-sdk-install`)
> resolve at runtime, exactly as this document's Packaging section always
> said it would once wired up. The macOS builder (`cargo ci build-tarballs`) and
> the Homebrew formula renderer (`cargo ci brew-formulas`) now both explicitly exclude
> `svbony-camera`: indi-3rdparty ships no confirmed `mac_arm64` blob, and
> that builder is arm64-only, so there is no real SDK to link there —
> shipping a `SVBONY_SKIP_NATIVE_LINK=1` (simulation-only) binary as the
> "real" nightly tarball would silently hand users a driver that can never
> see a physical camera. The Bazel-side SDK-fetch rule (`:svbony-camera`'s
> `manual` tag) remains separately deferred, unchanged by this fix — see
> "This phase's link-gating shortcut" below.
>
> **Status:** **Phase G landed (2026-07-21, this is the final planned
> phase): packaging + real-hardware validation marked pending.**
> `svbony-camera` is now **v0-complete pending real-hardware validation**
> against a physical SV605CC (on order, not yet arrived in this
> environment) — see "Real-hardware validation" below for the specific open
> items. Packaging landed: a new
> [`rusty-photon-svbony-sdk-install`](../../../services/svbony-camera/pkg/rusty-photon-svbony-sdk-install)
> root-only helper (mirroring
> [`rusty-photon-qhy-firmware-install`](../../../services/qhy-camera/pkg/rusty-photon-qhy-firmware-install))
> downloads the pinned SDK blob from the same indi-3rdparty commit
> `install-svbony-sdk` (CI) uses, verifies a real (curled + sha256-summed,
> not fabricated) pinned sha256 per architecture (amd64/armv8), and installs
> `libSVBCameraSDK.so` to `/usr/lib/rusty-photon/` — **this phase's one open
> technical decision, now resolved: RUNPATH, matching `zwo-camera`'s
> mechanism.** See "Packaging" below for the reasoning (in short: the SDK
> blob has no embedded SONAME either way, per Phase F's byte-inspection, but
> `/usr/lib/rusty-photon/` is a private package-owned directory outside
> `ldconfig`'s default scan path, so the packaged binary needs
> `-Wl,-rpath,/usr/lib/rusty-photon` baked in at build time — the exact
> mechanism `scripts/build-packages.sh` already applies for `zwo-camera`'s
> bundled, also-SONAME-less blob). `pkg/postinst`/`pkg/postrm` now exist
> (new this phase, following `qhy-camera`'s download-on-target shape, not
> `zwo-camera`'s bundled shape), and the `Cargo.toml` `[package.metadata.deb]`/
> `[package.metadata.generate-rpm]` sections' `TODO Phase G` markers are
> filled in (asset entries for the new helper, explicit `depends`/`requires`
> instead of `$auto`/rpm auto-detection — the operator-installed SONAME-less
> blob has exactly zwo-camera's dpkg-shlibdeps/rpm-auto-req problem even
> though this package never bundles it). **Deliberately NOT done this
> phase** (out of the explicit scope handed down for this phase, tracked as
> follow-up): a Bazel-side SDK-fetch repository rule (the `manual` tag on
> `:svbony-camera` and `libsvbony-sys/BUILD.bazel`'s unconditional
> `SVBONY_SKIP_NATIVE_LINK=1` stay exactly as Phase F left them — packaging
> here is purely a Cargo/`cargo-deb`/`cargo-generate-rpm` concern, verified
> by reading `zwo-camera`'s/`qhy-camera`'s `BUILD.bazel` files: neither has
> any packaging-related Bazel wiring beyond the plain `rust_library`/
> `rust_binary`/`rust_test` targets already present), and
> `scripts/build-packages.sh` staging/RUNPATH-injection support for
> `svbony-camera` specifically (today that script would still fail to
> produce a real `rusty-photon-svbony-camera` package — it has no
> `needs_svbony` SDK-staging leg the way it does for QHY/ZWO — since it
> already discovers `svbony-camera` via its `pkg/` directory but has no
> SVBony-specific staging block; adding one is mechanical follow-up in the
> same bucket as the Bazel SDK-fetch rule, not attempted here since it
> cannot be tested without the real SDK either). **The `build-packages.sh`
> half of this landed separately as issue #679** (see the banner at the top
> of this document) — the Bazel-side SDK-fetch rule remains deferred.
>
> **Phase F landed (2026-07-21): ConformU + CI gates.**
> `tests/conformu_integration.rs` now exists (mirrors `zwo-camera`'s: starts
> the production binary built with `--features conformu`, which pulls in the
> `simulation` backend so the SDK yields one `SV605CC-Simulated` camera, and
> runs ASCOM ConformU against it — self-skipping when `CONFORMU_PATH` is
> unset), with the matching `[package.metadata.conformu]` in `Cargo.toml` and
> a Bazel `conformu_integration` target (`tags = ["conformu"]`, excluded from
> the default `bazel test //...` gate; run with `bazel test --config=conformu
> //services/svbony-camera:conformu_integration`). A new
> [`.github/actions/install-svbony-sdk`](../../../.github/actions/install-svbony-sdk/action.yml)
> composite action (mirroring `install-zwo-sdk`) provisions the real SVBony
> SDK from a pinned indi-3rdparty commit, wired into `conformu.yml` (Linux +
> macOS x86_64; the Windows per-service matrix ran svbony-camera against the
> `simulation` backend with `SVBONY_SKIP_NATIVE_LINK` until 2026-07-29, when
> the private CI mirror gave it the real link too — see "Windows" below) and
> `native.yml` (the nightly real-link build + `svbony-rs` FFI smoke tests). See "Native dependency & build
> gating" below for what did and did not change under Bazel, and "Delivery
> phasing" for the full rundown incl. two bonus findings (no embedded SONAME
> in the vendored blob despite the CMakeLists' `SOVERSION` property; a
> pre-existing `SVBONY_SKIP_NATIVE_LINK` gap in four nightly Cargo
> safety-net workflows, fixed alongside this phase).
>
> **Phase E landed (2026-07-21): full `Camera` implementation.**
> `services/svbony-camera` builds, binds the Alpaca listener on port
> **11125**, and serves `/management/*` correctly with zero or one
> registered device; `--config`/`--port`/`--log-level` and the `doctor`
> subcommand all work.
> [`SvbonyCamera`](../../../services/svbony-camera/src/camera.rs) implements
> both `ascom_alpaca::api::Device` and `ascom_alpaca::api::Camera` for real:
> connection lifecycle, setup pages, sensor geometry/type, gain/offset/
> readout, binning/ROI, cooling, and the soft-trigger video-capture exposure
> state machine (incl. abort and pulse-guide) are all backed by
> [`backend::CameraHandle`](../../../services/svbony-camera/src/backend.rs)
> over `svbony-rs`. The one permanent stub is `ElectronsPerADU`
> (`NOT_IMPLEMENTED`, ST2 — no native SDK field). With the `simulation`
> feature the server registers `svbony-rs`'s one fabricated
> `SV605CC-Simulated` camera as "camera device 0" so BDD scenarios have a
> real device to address; the production (real-SDK) build registers **zero**
> devices by default in this phase — see "Configuration → Device
> registration boundary". All nine BDD feature files are genuinely green
> (60 scenarios, 242 steps); E9 (mid-exposure SDK failure / exceeded
> `SVBGetVideoData` deadline), the generation-counter abort-race and E10
> (reconnect during an exposure) are covered by unit tests instead, per the
> design's own call (the simulation cannot force an SDK error, and the
> reconnect race needs a capture held at a point no client can steer it to). See "Delivery phasing" for
> what Phase E resolved vs. left open for Phase G hardware validation.

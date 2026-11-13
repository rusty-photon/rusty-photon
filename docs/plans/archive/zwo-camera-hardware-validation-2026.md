# zwo-camera: the real-hardware runs, 2026-06 to 2026-08

A record, moved whole from `docs/services/zwo-camera.md` on 2026-11-13: the
dated account of the runs against physical ZWO cameras — the first ConformU
passes on real hardware (2026-06-20), the readout-format measurements on the
ASI120MC-S, the delivered ceilings of the ASI178MM and the ASI1600MM-Cool and
three cameras on one service (2026-08-05), the shared camera-core crate on
hardware (2026-08-07), the reconnect contract on hardware (2026-08-23), the
recorded re-runs on Linux and Windows (2026-07-27), and three simultaneous
ConformU runs against one service (2026-06-21). It stood under
[zwo-camera.md § Testing](../../services/zwo-camera.md#testing), whose
*Real-hardware validation* lists the ConformU records in
[`docs/validation/`](../../validation/README.md) with what each settled. The
sections and contracts this record names in italics or by label (ST3,
RM1-RM4, C0, C4, E10, …) are that document's, and state what the service does
today.

## The runs

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
[docs/validation/2026-08-05-zwo-camera-asi178mm-maxadu/](../../validation/2026-08-05-zwo-camera-asi178mm-maxadu/README.md).

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
[docs/validation/2026-08-05-zwo-camera-asi1600mm-cool-maxadu/](../../validation/2026-08-05-zwo-camera-asi1600mm-cool-maxadu/README.md).

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
[`rusty-photon-camera-core`](../../../crates/rusty-photon-camera-core/). Both
suites clean on all three; full record in
[docs/validation](../../validation/2026-08-07-zwo-camera-three-cameras-linux/README.md).
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
[docs/validation](../../validation/2026-08-23-zwo-camera-three-cameras-reconnect/README.md).
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
the [hardware validation record trail](../../validation/README.md), so their
ConformU output was not preserved. Recorded re-runs against the same physical
ASI1600MM-Cool exist for both platforms, both fully clean:
the [Linux record](../../validation/2026-07-27-zwo-camera-asi1600mm-cool-linux/README.md)
(ConformU 4.3.0) and the
[Windows record](../../validation/2026-07-27-zwo-camera-asi1600mm-cool-windows/README.md)
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

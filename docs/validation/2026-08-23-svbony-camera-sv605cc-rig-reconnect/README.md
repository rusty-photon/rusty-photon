# svbony-camera — reconnect-during-exposure fix on the field rig, SV605CC (2026-08-23)

Real-hardware validation of the change under test in
[PR #1062](https://github.com/rusty-photon/rusty-photon/pull/1062): the
`svbony-camera` half of contract **E10**, the reconnect-during-exposure case
filed as [#889](https://github.com/rusty-photon/rusty-photon/issues/889) and
fixed for `zwo-camera` in
[PR #1056](https://github.com/rusty-photon/rusty-photon/pull/1056). The PR adds
the two E10 properties this driver was missing — an `open_epoch` gate on every
SDK call a capture makes after configuring its frame, and an `Arc::ptr_eq`
ownership check before a drain releases the in-flight slot.

The exposure state machine is what the PR rewired, so this run exists to put
abort, disconnect and reconnect on the physical camera — the paths the
simulation can only approximate — and to check that a driver-internal change to
how captures are cancelled did not alter what a conformance validator sees.

## What was tested

| | |
|---|---|
| Commit | `74aa87e6` (branch `fix/svbony-camera-capture-instance-gate`, PR #1062) |
| Platform | Raspberry Pi 5 (BCM2712, aarch64), Raspberry Pi OS / Debian 13 trixie, kernel 6.18.34+rpt-rpi-2712 — the telescope field rig |
| Binary | `cargo run --release -p svbony-camera` — **default features**, i.e. the production non-`simulation` path `svbony-camera → svbony-rs → libsvbony-sys → libSVBCameraSDK.so`; built on the rig with rustc 1.97.0 |
| SDK | `libSVBCameraSDK.so` v1.13.4 `armv8` at `/usr/lib/rusty-photon`, placed by the packaged `rusty-photon-svbony-sdk-install` helper (unchanged since the 2026-07-30 record) |
| udev | packaged `90-rusty-photon-svbony.rules` (`root:rusty-photon` 0660, `usbfs_memory_mb=200`); this instance runs as the login user, granted the device node for the duration by a temporary ACL |
| Camera | SVBONY SV605CC, hardware serial `0123481353808C03EE2512150035` (the same physical unit as every prior record), direct on a Pi USB 3 port |
| Instance | one service, port 11135, plain HTTP on the rig's loopback, started from a pristine working directory holding only its config file; the packaged unit and `sentinel` stopped for the duration and restored afterwards |
| ConformU | 4.5.0 (Build 53834.49ab847), x86_64 dev box, reaching the service through an ssh port-forward |
| Reference | packaged nightly `0.1.0+nightly.202608170258.g9785452` — the same driver **without** this PR, used for every A/B below |

The production TLS + HTTP Basic endpoint was not the target: its transport path
is untouched by this change and was proven by the
[2026-08-08 record](../2026-08-08-svbony-camera-sv605cc-rig/README.md).

## Verdicts

- **`alpacaprotocol`** — *"no errors, issues or information alerts"*:
  [alpacaprotocol.log](alpacaprotocol.log). Zero information alerts, matching
  every record since the #891 connect handshake.
- **`conformance`** — *"no errors, warnings or issues found"* and *"all members
  returned within their target response times"*: [conformance.log](conformance.log),
  machine-readable [conformance-results.json](conformance-results.json)
  (`ErrorCount` / `IssueCount` / `ConfigurationAlertCount` / `TimingIssuesCount`
  all 0, across 83 timed members).

## What the camera reported

| | |
|---|---|
| Sensor, reported | 2976 × 3000 (8.9 MPix) |
| Sensor type | RGGB (colour), `BayerOffsetX/Y` 1 / 0 |
| `MaxBinX/Y` | 4 (full frames captured at 1×1, 2×2, 3×3, 4×4) |
| `PixelSizeX/Y` | 3.76 µm |
| `MaxADU` | 65535 |
| Gain range | 0–600 |
| Offset range | 0–100 |
| Readout modes | `Raw16`, `Raw8`; `ReadoutMode` 0, index-consistent |
| `ExposureMin` / `ExposureMax` | 6.6e-05 s / 1999.999996 s |
| `InterfaceVersion` | 4 |
| `CCDTemperature` | 25.5–26.1 °C (ambient, TEC off) |
| `ElectronsPerADU`, `FullWellCapacity` | not implemented (unchanged) |

Every figure matches the [2026-08-08 record](../2026-08-08-svbony-camera-sv605cc-rig/README.md),
which is the point: the change is internal to how a capture is cancelled, and
nothing a client can observe about geometry, signal or cooling moved.

`CoolerOn` read `false` on connect, and after ConformU had exercised the cooler
the TEC was confirmed off again — `CoolerOn = false`, `CoolerPower = 0` — so
tenet 3 held and the run left the hardware as it found it.

## The exposure paths this PR rewired

Driven over Alpaca from the rig itself (loopback, so the timings are the
driver's and not the WiFi link's), in addition to ConformU. **13/13 checks.**

| Check | Result |
|---|---|
| Connect leaves the cooler off (tenet 3) | ✓ `CoolerOn = false` |
| `CanStopExposure = false` (no data-preserving stop) | ✓ |
| Plain 0.5 s / 2 s exposures return their own geometry | ✓ 512 × 512, `LastExposureDuration` 0.5000 / 2.0000 |
| **E7** `AbortExposure` discards the frame, device idle | ✓ `Idle` within 0.01 s, `ImageReady = false` |
| A new exposure is accepted right after an abort | ✓ **0.11 s** — the drain hands the device back inside one 250 ms poll slice |
| `StopExposure` stays `NotImplemented` | ✓ `ErrorNumber 1024` |
| **C3** disconnect cancels the in-flight exposure | ✓ returns in 0.12 s; after reconnect `Idle`, `ImageReady = false` |
| A normal exposure works after abort/stop/disconnect | ✓ |
| **E10** sweep across integration and readout | ✓ **12/12** |
| **E10** soak at the integration/readout boundary | ✓ **30/30** |
| Full-frame exposure after the sweep | ✓ 2976 × 3000 |
| Run leaves the cooler as it found it | ✓ `CoolerOn = false`, `CoolerPower = 0` |

**E10 itself** was swept the same way as the zwo record: start a full-frame 3 s
exposure, disconnect part-way, reconnect, then start a *differently sized*
exposure (256 × 256) and require it to complete on time with its own geometry
and its own `LastExposureDuration`. The size difference is the tell — a frame
produced by the superseded capture carries the first exposure's dimensions, and
a superseded capture that released the in-flight slot would let the new exposure
be reported finished early. Twelve disconnect points from 0.2 s to 4.0 s, which
on this camera spans integration (`CameraState = Exposing` through 2.9 s),
readout (still `Exposing` at 3.0–3.6 s) and past completion (`Idle` at 4.0 s):
**12/12**, then **30/30** at the 2.9 s boundary. Every trial came back
256 × 256 with `LastExposureDuration = 3.0000`, walls tightly clustered at
4.31 s (one 7.43 s outlier).

## What this run could not show, and why that is expected

**The #889 race does not reproduce through the Alpaca API on a healthy box, on
either side of the fix** — the same finding the zwo record reached, for a
structurally similar reason. The pre-PR packaged binary was driven through the
identical harness on the same camera: **13/13, sweep 12/12, soak 30/30**, all
timings within noise of the fixed build.

It was then starved deliberately — service pinned to one core, that core
crowded with 64 spinners, load average 65. The starvation bit: the plain wall
time per trial went 4.31 s → 7.25 s, the post-abort re-accept 0.11 s → 1.12 s,
the disconnect 0.12 s → 1.16 s. The window still never opened: **13/13, sweep
12/12, soak 30/30** under load.

That is structural rather than luck, and this driver's shape says why. A
capture's poll read holds the camera lock for the duration of its
`SVBGetVideoData` slice, and `set_connected(false)` sets the capture's cancel
flag *before* it takes that lock to close the handle — so by the time the close
can proceed, the flag the capture checks the instant it releases the lock is
already set. To reach the new camera at all, the superseded capture has to be
preempted in the sliver between releasing the lock and reading the flag, and
stay preempted for the whole reconnect plus the next `StartExposure` — measured
here at 1.3–2.6 s of SDK work. Uniform CPU starvation stretches both sides, so
it does not flip the ratio.

So the hardware evidence is **no regression across every rewired path**, plus a
severity calibration: the defect is real but needs a badly starved capture
thread. The deterministic proof lives in the unit tests, each verified to fail
against the pre-fix shape:

- `production_handle_capture_does_not_read_from_a_reopened_camera` — pre-fix the
  capture polled `SVBGetVideoData` on the camera the reconnect opened under it.
- `production_handle_abort_does_not_discard_a_reopened_cameras_frame` — pre-fix
  the drain's `SVBStopVideoCapture` discarded the *new* exposure's frame, which
  on this SDK is unrecoverable (there is no data-preserving stop).
- `a_superseded_capture_does_not_release_the_new_exposures_slot` — pre-fix
  `CameraState` read `Idle` while the second exposure was still running.

## A pre-existing defect this run surfaced

**The first exposure of a session fails if it is shorter than about 2.1 s.**
Reproduced deterministically on *both* binaries, so it is not from this PR:

| Case | Verdict |
|---|---|
| Connect, then a 0.5 s exposure | **`CameraState = Error`**, 3/3 reps, at ~2.65 s |
| Connect, change the ROI, then a 0.5 s exposure | **`Error`**, 3/3 reps, at ~2.66 s |
| Mid-session, change the ROI, then a 0.5 s exposure | ok, 3/3 reps, 1.65 s |
| Connect, change the ROI, then a 2.0 s exposure | ok, 3/3 reps, 3.32 s |

The first `SVBGetVideoData` of a session carries ~2.6 s of fixed SDK overhead
(buffer allocation and USB setup), and the driver's deadline for the whole
read is `exposure * 2 + 500 ms` — 1.5 s for a 0.5 s exposure. The read
therefore times out and the exposure lands in `Error` (state-machine step 7 /
E9) even though nothing is wrong. Any client that opens a session and takes a
short exposure first hits this on every connect; ConformU does not, because its
first exposure is long enough to cover the overhead. Filed as
[#1067](https://github.com/rusty-photon/rusty-photon/issues/1067) rather than
fixed here, to keep this PR to the reconnect contract.

## Files

- [alpacaprotocol.log](alpacaprotocol.log)
- [conformance.log](conformance.log)
- [conformance-results.json](conformance-results.json)

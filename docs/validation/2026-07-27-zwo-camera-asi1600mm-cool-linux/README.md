# zwo-camera on Linux — ASI1600MM-Cool, 2026-07-27

Recorded Linux ConformU run against the physical ASI1600MM-Cool. The original
Linux validation (2026-06-20, narrated in
[docs/services/zwo-camera.md](../../services/zwo-camera.md)
"Real-hardware validation") predated this record trail, so its ConformU
output was not preserved; this run pins the evidence at current `main`,
on the same physical camera and host, five weeks later.

## What was tested

| | |
|---|---|
| Commit | [`e0281daf`](https://github.com/rusty-photon/rusty-photon/commit/e0281daf) (`origin/main` at test time) |
| Service | `zwo-camera`, **real-SDK** build (default features, no `ZWO_SKIP_NATIVE_LINK`) |
| Build | `cargo build --release -p zwo-camera`; rustc 1.96.0; `libzwo-sys` links `ASICamera2` from `/usr/local/lib` (its default Linux search path) |
| SDK | `libASICamera2.so` **v1.41** (`ASIGetSDKVersion` = `1, 41, 0, 0`) at `/usr/local/lib` |
| Platform | Fedora Linux 44 (Workstation), x86_64 — camera on the host's own USB 3 bus (no VM). `99-asi.rules` udev rule (VID `03c3` `MODE="0666"` + `usbfs_memory_mb` raised to 256 for the ~32 MB RAW16 full frame) |
| Camera | ZWO ASI1600MM-Cool (cooled, mono, 12-bit). The model exposes neither a hardware serial nor a flash ID, so it takes the documented position-based identity fallback: UniqueID `ZWO:ZWO-ASI1600MM-Cool:noserial-0` |
| ConformU | 4.3.0 build 49708, run on the same host against `http://127.0.0.1:11122/api/v1/camera/0` |

## Verdicts

- **`alpacaprotocol`** — **0 errors, 0 issues, 0 information alerts**:
  [alpacaprotocol.log](alpacaprotocol.log). (Unlike the SVBONY SV605CC
  records, the ASI SDK accepts ConformU's `PUT Gain` casing probes, so there
  are no informational items at all.)
- **`conformance`** — *"no errors, warnings or issues found"*, and every
  member inside its response-time target: [conformance.log](conformance.log),
  machine-readable [conformance-results.json](conformance-results.json)
  (`ErrorCount`/`IssueCount`/`ConfigurationAlertCount`/`TimingIssuesCount`
  all 0)
- Key values match the 2026-06-20 narrative exactly: sensor 4656×3520
  reported as **4608×3504** (R4 alignment), `MaxADU` 4095 (12-bit),
  `SensorType` Monochrome, gain 0–600, offset 0–100, cooled
  (`CanSetCCDTemperature`/`CanGetCoolerPower` true), ST4 `CanPulseGuide`,
  both `AbortExposure` and `StopExposure`.
- **Cross-platform SDK discrepancy** (issue
  [#741](https://github.com/rusty-photon/rusty-photon/issues/741)):
  `ElectronsPerADU` reads **0.00496** here but **4.96** through the Windows
  v1.41 DLL on the same camera
  ([2026-07-27 Windows record](../2026-07-27-zwo-camera-asi1600mm-cool-windows/README.md))
  — an exact 1000× split between ZWO's two v1.41 SDK blobs. The Windows
  value is the physically plausible one for an ASI1600 at gain 0; the driver
  reports the SDK's `ASI_CAMERA_INFO.ElecPerADU` verbatim on both platforms.

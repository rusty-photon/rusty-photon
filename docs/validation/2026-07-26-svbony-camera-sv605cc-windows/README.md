# svbony-camera on Windows — SV605CC, 2026-07-26

First real-hardware validation of `svbony-camera` on Windows
([issue #720](https://github.com/rusty-photon/rusty-photon/issues/720) Part 1),
mirroring the Linux SV605CC validation recorded in
[docs/services/svbony-camera.md](../../services/svbony-camera.md)
("Real-hardware validation").

## What was tested

| | |
|---|---|
| Commit | [`ef03a1cd7b9e0831e731d0ed9d37df7661fe5edd`](https://github.com/rusty-photon/rusty-photon/commit/ef03a1cd7b9e0831e731d0ed9d37df7661fe5edd) (`origin/main` at test time, fresh clone) |
| Service | `svbony-camera`, **real-SDK** build (default features, no `SVBONY_SKIP_NATIVE_LINK`) |
| Build | `cargo build --release -p svbony-camera` with `SVBONY_SDK_LIB_DIR=<SDK>\lib\x64`; Rust 1.97.1 `stable-x86_64-pc-windows-msvc`, MSVC 14.44 (VS 2022 Build Tools) |
| SDK | `SVBCameraSDK.dll` x64 v1.13.4 (`windows-SVBCameraSDK-v1.13.4.zip`, SVBony's own download; same SDK version as the Linux pin), placed next to the exe |
| Device driver | SVBony Windows camera driver v1.0.0.8 (`SVBONY-Driver-DS-V1.13.4-20250205.exe`) |
| Platform | Windows 11 Pro 25H2 (build 26200), x86_64 — a KVM/QEMU guest with the camera attached via USB passthrough (`qemu-xhci`) |
| Camera | SVBONY SV605CC, hardware serial `0123481353808C03EE2512150035` (the same physical unit as the Linux validation) |
| ConformU | 4.4.0 build 52526, run on the same machine against `http://127.0.0.1:11125/api/v1/camera/0` |

## Verdicts

- **`alpacaprotocol`** — *"no errors, issues or information alerts"*:
  [alpacaprotocol.log](alpacaprotocol.log)
- **`conformance`** — *"no errors, warnings or issues found"*, and every
  member inside its response-time target: [conformance.log](conformance.log),
  machine-readable [conformance-results.json](conformance-results.json)
  (`ErrorCount`/`IssueCount`/`ConfigurationAlertCount`/`TimingIssuesCount`
  all 0)
- **`UniqueID` minting matches Linux exactly**:
  `SVBONY:SVBONY-SV605CC:0123481353808C03EE2512150035`

## Sharp paths re-exercised on Windows

The paths the Linux validation found sharp, driven over the Alpaca API
against the same binary and commit in the same session:

| Path | Result |
|------|--------|
| `SVB_EXPOSURE` in µs | Confirmed — a 3 s request integrates ~3.4 s wall-clock to `ImageReady` |
| `MaxADU` | 65535 (full-16-bit rescaled Raw16, as on Linux) |
| R4 aligned-down sensor extents | `CameraXSize`/`CameraYSize` = 2976×3000 |
| Responsive abort | Abort → next exposure *accepted* in ~0.08 s (Linux: ~0.3 s); recovery frame clean |
| Full-frame transfer | `ImageBytes` payload exactly 17 856 044 bytes (2976×3000×2 + 44). The Linux `usbfs_memory_mb` bump has **no Windows analogue and none is needed** — the ~18 MB frame passes even through QEMU USB passthrough |
| Cooling (workspace tenet 3) | Connect leaves the TEC untouched; a setpoint alone does not engage it; `CoolerOn=true` ramps power (6→9 %) with falling sensor temperature; `CoolerOn=false` returns power to 0 |
| Config materialization | Default config self-materializes at `%ProgramData%\rusty-photon\svbony-camera.json` on first start |

## Windows-specific findings

- **The camera needs SVBony's driver package; WinUSB does not auto-bind.**
  The SV605CC advertises no Microsoft OS descriptors (its compatible IDs
  are bare `USB\Class_FF`), so until
  `SVBONY-Driver-DS-V1.13.4-20250205.exe` installs the vendor INF
  (v1.0.0.8) the device sits at problem code 28 and the SDK enumerates
  zero cameras. Like the SDK zip, the driver download is behind
  svbony.com's captcha gate — relevant to #720 Part 2 (CI provisioning).
- **The SDK's parameter dumps land in `%APPDATA%\CKConfig\`**
  (`U3SM900C-AST_Cfg_A.bin` / `_Cfg_SAVE.bin`) — *not* in the process
  working directory as on Linux, so the Linux CWD-writability concern has
  no Windows analogue.

Operator-facing setup steps live in
[docs/svbony-camera-windows-install.md](../../svbony-camera-windows-install.md).

# zwo-camera on Windows — ASI1600MM-Cool, 2026-07-27

**First-ever Windows real-hardware validation of `zwo-camera`** — the first
time the `libzwo-sys` Windows link directives drove a physical camera (CI's
`native.yml` link-checks the Windows build but has no hardware). Same
physical ASI1600MM-Cool as the
[Linux record](../2026-07-27-zwo-camera-asi1600mm-cool-linux/README.md),
run the same day.

## What was tested

| | |
|---|---|
| Commit | [`1f2b9d16`](https://github.com/rusty-photon/rusty-photon/commit/1f2b9d16) (`origin/main` at test time; no zwo-crate changes vs the Linux record's `e0281daf`) |
| Service | `zwo-camera`, **real-SDK** build (default features, no `ZWO_SKIP_NATIVE_LINK`) |
| Build | `cargo build --release -p zwo-camera` with `ZWO_SDK_LIB_DIR` pointing at the staged SDK `lib\x64`; `ASICamera2.dll` copied next to the exe; rustc 1.97.1 (stable-msvc), VS Build Tools MSVC 14.44. `libzwo-sys` runs bindgen at build time, so libclang is required on Windows — LLVM 20.1.8 installed, `LIBCLANG_PATH=C:\Program Files\LLVM\bin` |
| SDK | `ASI_Windows_SDK_V1.41.zip` (nested inside ZWO's developer camera SDK download, `dl.zwoastro.com/software?app=DeveloperCameraSdk&platform=windows86` — the same rolling-"latest" URL `.github/actions/install-zwo-sdk` uses): `ASICamera2.lib` + `ASICamera2.dll` from `lib\x64` |
| Driver | **ZWO's native camera driver is required**: the camera carries no MS OS descriptors (Device Manager code 28, bare `USB\Class_FF` compatible IDs), so WinUSB never auto-binds — same situation as the SVBONY SV605CC. `ZWO_ASI_Cameras_driver_Setup_V3.28.0.0.exe` (NSIS, `/S` silent-installs) binds the camera as `ZWO ASI1600MM COOL Camera`, driver v3.28.0.0 (`asicamusb3.inf`, provider ZWO, device class Image). Unlike SVBony's, the driver is a captcha-free direct download (`dl.zwoastro.com/software?app=AsiCameraDriver&platform=windows86` — note: the endpoint rejects HTTP `HEAD`, fetch with a plain `GET`) |
| Platform | Windows 11 (25H2, build 26200) x64 — a KVM/QEMU guest with the camera passed through on an emulated `qemu-xhci` USB 3 controller. The ~32 MB RAW16 full frame (4608×3504×2) transfers through the passthrough without issue |
| Camera | ZWO ASI1600MM-Cool — identical UniqueID minting to Linux: `ZWO:ZWO-ASI1600MM-Cool:noserial-0` (the model has no hardware serial or flash ID) |
| ConformU | 4.4.0.52526, run inside the guest against `http://127.0.0.1:11122/api/v1/camera/0` |

## Verdicts

- **`alpacaprotocol`** — **0 errors, 0 issues, 0 information alerts**:
  [alpacaprotocol.log](alpacaprotocol.log) (like Linux, the ASI SDK accepts
  the `PUT Gain` casing probes — no informational items)
- **`conformance`** — *"no errors, warnings or issues found"*, and every
  member inside its response-time target: [conformance.log](conformance.log),
  machine-readable [conformance-results.json](conformance-results.json)
  (`ErrorCount`/`IssueCount`/`ConfigurationAlertCount`/`TimingIssuesCount`
  all 0)
- Values match the Linux record (4608×3504 R4-aligned extents, `MaxADU`
  4095, Monochrome, cooled, ST4) — with one exception:
- **Cross-platform SDK discrepancy** (issue
  [#741](https://github.com/rusty-photon/rusty-photon/issues/741)):
  `ElectronsPerADU` reads **4.96** here but **0.00496** through the Linux
  v1.41 blob on the same camera — an exact 1000× split between ZWO's two
  v1.41 SDK builds. This (~4.96 e⁻/ADU) is the physically plausible value
  for an ASI1600 at gain 0; the driver reports the SDK's
  `ASI_CAMERA_INFO.ElecPerADU` verbatim on both platforms.

## Windows-specific findings

- The packaged `rusty-photon-zwo-camera` Windows service (from the nightly
  MSI) was already installed and holding port 11122 in the test guest; it —
  and the sentinel supervising it — were stopped for the run and restarted
  afterwards. Operators testing a locally-built binary on a machine with the
  MSI stack installed must do the same.
- No Windows analogue of the Linux `usbfs_memory_mb` bump exists or was
  needed; the vendor driver handles bulk-transfer sizing itself.

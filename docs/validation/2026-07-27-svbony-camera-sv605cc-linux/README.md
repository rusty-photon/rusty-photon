# svbony-camera on Linux — SV605CC, 2026-07-27

Recorded Linux ConformU run against the physical SV605CC. The original
Linux validation (2026-07-26, narrated in
[docs/services/svbony-camera.md](../../services/svbony-camera.md)
"Real-hardware validation") predated this record trail, so its ConformU
output was not preserved; this run pins the evidence at current `main`,
on the same physical camera and host, one day later.

## What was tested

| | |
|---|---|
| Commit | [`bdd97201`](https://github.com/rusty-photon/rusty-photon/commit/bdd97201) (`origin/main` at test time) |
| Service | `svbony-camera`, **real-SDK** build (default features, no `SVBONY_SKIP_NATIVE_LINK`) |
| Build | `cargo build --release -p svbony-camera` with `SVBONY_SDK_LIB_DIR` pointing at the staged SDK blob; rustc 1.96.0 |
| SDK | `libSVBCameraSDK.so` v1.13.4 amd64 from the pinned indi-3rdparty blob — sha256 `371bcf7f…` verified identical to `rusty-photon-svbony-sdk-install`'s `SHA256_AMD64` pin; resolved at runtime via `LD_LIBRARY_PATH` |
| Platform | Fedora Linux 44 (Workstation), x86_64 — camera on the host's own USB 3 bus (no VM). Dev udev rule per issue #710's helper shape (`MODE="0666"` on VID `f266` + `usbfs_memory_mb` raised to 256 for the ~18 MB full frame) |
| Camera | SVBONY SV605CC, hardware serial `0123481353808C03EE2512150035` (the same physical unit as the Windows record) |
| ConformU | 4.3.0 build 49708, run on the same host against `http://127.0.0.1:11125/api/v1/camera/0` |

## Verdicts

- **`alpacaprotocol`** — **0 errors, 0 issues**, 4 information messages:
  [alpacaprotocol.log](alpacaprotocol.log). The 4 informational items are
  the suite's `PUT Gain` casing probes drawing an in-protocol ASCOM
  `InvalidOperationException` ("SVBony camera SDK error: general error")
  — HTTP semantics correct, so ConformU classifies them as informational.
  The identical four items appeared on the first Windows run
  ([2026-07-26 record](../2026-07-26-svbony-camera-sv605cc-windows/README.md)),
  so this is cross-platform SDK behaviour around gain writes on a freshly
  connected camera, not a platform quirk; direct gain writes across the
  full 0–600 range succeed on both platforms.
- **`conformance`** — *"no errors, warnings or issues found"*, and every
  member inside its response-time target: [conformance.log](conformance.log),
  machine-readable [conformance-results.json](conformance-results.json)
  (`ErrorCount`/`IssueCount`/`ConfigurationAlertCount`/`TimingIssuesCount`
  all 0)
- **`UniqueID`**: `SVBONY:SVBONY-SV605CC:0123481353808C03EE2512150035` —
  identical minting on both platforms.

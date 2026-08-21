# svbony-camera on the field rig — SV605CC, 2026-07-30

First validation of the **packaged** `rusty-photon-svbony-camera`
against the physical camera on the deployment target itself — the Pi 5
telescope field rig, installed from the nightly apt repo. This closes
the last open item from the Linux dev-box validation
([docs/services/svbony-camera.md](../../services/svbony-camera.md)
"Real-hardware validation"): the dev-box and Windows runs exercised
locally built binaries; this run exercises the shipped deb, the
operator SDK-install helper, and the packaged udev rule end-to-end.

## What was tested

| | |
|---|---|
| Commit | [`05d74aab`](https://github.com/rusty-photon/rusty-photon/commit/05d74aab) — nightly `0.1.0+nightly.202607300739.g05d74aa`, installed via the nightly apt repo |
| Service | `rusty-photon-svbony-camera` **packaged arm64 deb** (real SDK), self-created default config (plain HTTP, `0.0.0.0:11125`) |
| SDK | `libSVBCameraSDK.so` v1.13.4 `armv8` blob, installed to `/usr/lib/rusty-photon` by the shipped `/usr/sbin/rusty-photon-svbony-sdk-install` (sha256 verified against the helper's pin); resolved via the binary's RUNPATH — the documented retry-until-SDK-lands startup was observed live (two 127 exits, then clean start) |
| udev | Packaged `90-rusty-photon-svbony.rules` in charge: device node `root:rusty-photon 0660`, `usbfs_memory_mb` raised to 200 |
| Platform | Raspberry Pi 5 (BCM2712, kernel `6.18.34+rpt-rpi-2712`, aarch64), Raspberry Pi OS (Debian 13 trixie) — the telescope field rig; camera direct on a Pi USB 3 port (5 Gbps) |
| Camera | SVBONY SV605CC, hardware serial `0123481353808C03EE2512150035` (the same physical unit as the dev-box and Windows records) |
| ConformU | 4.4.0 build 52526, run from the x86_64 dev box over WiFi LAN against `http://10.0.85.245:11125/api/v1/camera/0` |

## Verdicts

- **`alpacaprotocol`** — **0 errors, 0 issues**, 4 information messages:
  [alpacaprotocol.log](alpacaprotocol.log). The 4 informational items are
  the same `PUT Gain` casing probes drawing an in-protocol ASCOM
  `InvalidOperationException` ("SVBony camera SDK error: general error")
  seen on every prior platform
  ([Linux 2026-07-27](../2026-07-27-svbony-camera-sv605cc-linux/README.md),
  [Windows 2026-07-26](../2026-07-26-svbony-camera-sv605cc-windows/README.md))
  — freshly-connected-camera SDK behaviour, not a rig or packaging quirk.
- **`conformance`** — *"no errors, warnings or issues found"*, and every
  member inside its response-time target: [conformance.log](conformance.log),
  machine-readable [conformance-results.json](conformance-results.json)
  (`ErrorCount`/`IssueCount`/`ConfigurationAlertCount`/`TimingIssuesCount`
  all 0). The R4-aligned full frame (2976 × 3000, ~18 MB Raw16) transferred
  over the WiFi link in 1.68 s; binned full frames at bins 2–4 clean.
- **`UniqueID`**: `SVBONY:SVBONY-SV605CC:0123481353808C03EE2512150035` —
  identical minting to both prior platforms, now from the packaged binary.

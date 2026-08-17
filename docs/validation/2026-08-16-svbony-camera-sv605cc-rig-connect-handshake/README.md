# svbony-camera connect handshake (C1a / GO5) on the field rig — SV605CC, 2026-08-16

Re-validation of the packaged `svbony-camera` after the fleet-wide upgrade
that carried the connect-handshake fix for issue
[#891](https://github.com/rusty-photon/rusty-photon/issues/891): the driver
now mirrors `indi_svbony_ccd`'s `Connect()` — `SVBRestoreDefaultParam`,
`SVBSetAutoSaveParam(false)`, then a manual `SVB_EXPOSURE` write that clears
the SDK's auto-exposure state — so `Gain` is settable before the first
exposure no matter what the process's working directory holds. That
mechanism had been established from the SDK binary and pinned against the
simulation's reproduction of the gate; this record is the pass against the
physical camera the design doc listed as the one open item, plus the ConformU
suites on the same instance.

The instance under test was started from a **pristine working directory** —
one containing only the service's config file, no `U3SM900C-AST_Cfg_SAVE.bin`
from any earlier session — because that is the exact scenario of #891: the
production unit's `/var/lib/rusty-photon` has carried the SDK's persisted
"auto-exposure off" state since 2026-07-30 and would have masked the gate the
way every writable-cwd run did.

## What was tested

| | |
|---|---|
| Commit | [`97854524`](https://github.com/rusty-photon/rusty-photon/commit/97854524) (`origin/main` at build time; includes the #891 fix, merge [`48dd0892`](https://github.com/rusty-photon/rusty-photon/commit/48dd0892)) |
| Service | packaged arm64 nightly deb `rusty-photon-svbony-camera 0.1.0+nightly.202608170258.g9785452` (on-demand `nightly-packages.yml` dispatch after the merge), installed in the same-session upgrade of all 18 fleet packages from `202608080551.gc530940` |
| SDK | `libSVBCameraSDK.so` v1.13.4 `armv8`, placed by the packaged `rusty-photon-svbony-sdk-install` helper (unchanged since the 2026-07-30 record) |
| udev | packaged `90-rusty-photon-svbony.rules` |
| Platform | Raspberry Pi 5 (BCM2712, aarch64), Raspberry Pi OS (Debian 13 trixie) — the telescope field rig; camera direct on a Pi USB 3 port |
| Camera | SVBONY SV605CC, hardware serial `0123481353808C03EE2512150035` (the same physical unit as every prior record) |
| Instance | the installed `/usr/bin/rusty-photon-svbony-camera`, run ad hoc as the `rusty-photon` service user (packaged unit stopped for the duration) from an empty directory holding only `{"server":{"port":11135,"bind_address":"127.0.0.1"}}`; plain HTTP on the rig's loopback, reached from the x86_64 dev box through an ssh port-forward |
| ConformU | 4.5.0 build 53834, x86_64 dev box |

The production TLS + HTTP Basic endpoint was not the target this time: its
transport path is untouched by the change and was proven by the
[2026-08-08 record](../2026-08-08-svbony-camera-sv605cc-rig/README.md).

## Verdicts

- **Direct probe of the fix, before and after**, same procedure — start
  from an empty directory, `PUT Connected=true`, then `PUT Gain=120` with
  no exposure taken in the session:
  - previously deployed binary (`gc530940`, extracted from the apt cache):
    `ErrorNumber 1035` *"failed to set gain: SVBony camera SDK error:
    general error (e.g. value out of valid range)"* — the #891 symptom;
    after one 0.1 s exposure the identical `PUT Gain=120` succeeded, the
    "until the first exposure" half of the report.
  - new binary (`g9785452`): `PUT Gain=120` → `ErrorNumber 0`, reads back
    `120`; `PUT Offset=30` → reads back `30`; `PUT Connected=false` →
    `200`. The service log carries no `warn!` from the handshake (the
    restore and the auto-save toggle both succeeded).
  - The SDK wrote `U3SM900C-AST_Cfg_A.bin` and a byte-identical
    `U3SM900C-AST_Cfg_SAVE.bin` **once, at connect** (the
    `SVBRestoreDefaultParam` write) and did not touch either at
    disconnect — mtimes and SHA-256 unchanged across the close, which is
    auto-save staying off.
- **`alpacaprotocol`** — *"no errors, issues or information alerts"*:
  [alpacaprotocol.log](alpacaprotocol.log). Zero information alerts from a
  pristine working directory: the four informational `PUT Gain` items every
  pre-2026-08-05 SV605CC record carried are gone by the fix, not by the
  state of the working directory.
- **`conformance`** — *"no errors, warnings or issues found"*, every member
  inside its response-time target: [conformance.log](conformance.log),
  machine-readable [conformance-results.json](conformance-results.json)
  (`ErrorCount`/`IssueCount`/`ConfigurationAlertCount`/`TimingIssuesCount`
  all 0). The suite's `Gain Write` and `Offset Write` checks (min, max, and
  both out-of-range rejections) ran before its first `StartExposure`. Full
  frames at bins 1×1 through 4×4, `MaxADU 65535`, readout modes
  `Raw16`/`Raw8`, interface version 4 — unchanged from the 2026-08-08 record.

## SDK ground truth learned this run

- **`SDK_LOG=yes` is not a safe standing diagnostic.** With the SDK's own
  trace mode enabled, `SVBCloseCamera` segfaults the process (exit status
  139) whenever the session took **no exposure** — old and new binary alike,
  pristine or populated working directory alike; a session that has taken an
  exposure closes cleanly, and without the variable every close is clean.
  Set it for a one-off connect probe only, and expect that process to die at
  disconnect. Its output goes to syslog (the journal on a systemd host, under
  the process name), not to the process's stdout/stderr.
- The trace does show the handshake as intended: `CameraSetAeState` →
  `CameraSetExposureTime:999999.000000` (the 1 s manual write, SDK-quantized)
  → `CameraSetTriggerMode`, then `CameraSetAnalogGain` for the first client
  `PUT Gain` — no exposure in between.
- `SVBRestoreDefaultParam` writes both cfg files, not only `_Cfg_A.bin`;
  after it, `Gain` reads `0` and `Offset` reads `0` on this camera.

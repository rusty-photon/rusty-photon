# svbony-camera readout formats on the field rig — SV605CC, 2026-08-05

Validation of the **readout-format ladder** ([issue #882](https://github.com/rusty-photon/rusty-photon/issues/882),
merged as [#884](https://github.com/rusty-photon/rusty-photon/pull/884) +
[#885](https://github.com/rusty-photon/rusty-photon/pull/885)) against the
physical SV605CC on the Pi 5 telescope field rig. This closes the one item
[docs/services/svbony-camera.md](../../services/svbony-camera.md) left open
when the feature merged: no real frame had ever come off the hardware in
8-bit, because the camera advertises `Raw16` and the negotiated default
therefore never selects `Raw8`.

## What was tested

| | |
|---|---|
| Commit | [`4b8b8179`](https://github.com/rusty-photon/rusty-photon/commit/4b8b8179) (`origin/main` at test time) |
| Service | `svbony-camera`, **real-SDK** build (default features, no `SVBONY_SKIP_NATIVE_LINK`), built on the rig: `cargo build --release -p svbony-camera` with `SVBONY_SDK_LIB_DIR=/usr/lib/rusty-photon`; rustc 1.97.0 `stable-aarch64-unknown-linux-gnu` |
| SDK | `libSVBCameraSDK.so` v1.13.4 `armv8`, the copy the packaged `rusty-photon-svbony-sdk-install` had already placed in `/usr/lib/rusty-photon` (locally built binaries resolve it via `LD_LIBRARY_PATH`, not RUNPATH) |
| Launch | transient systemd unit as `User=rusty-photon`, **`WorkingDirectory=/var/lib/rusty-photon`** — required, see "Environment finding" below |
| udev | packaged `90-rusty-photon-svbony.rules` (device node `root:rusty-photon 0660`, `usbfs_memory_mb` 200) |
| Platform | Raspberry Pi 5 (BCM2712, kernel `6.18.34+rpt-rpi-2712`, aarch64), Raspberry Pi OS (Debian 13 trixie) — the telescope field rig; camera direct on a Pi USB 3 port |
| Camera | SVBONY SV605CC, hardware serial `0123481353808C03EE2512150035` (the same physical unit as every prior record) |
| ConformU | 4.4.0 build 52526, run from the x86_64 dev box over an **SSH tunnel to the rig's loopback**, so the driver stayed bound to `127.0.0.1` and was never exposed on the LAN |

## Verdicts

- **`alpacaprotocol`** — *"no errors, issues or information alerts"*:
  [alpacaprotocol.log](alpacaprotocol.log). Note **zero information
  alerts**: every prior SV605CC record carries four informational
  `PUT Gain` casing items, attributed there to "freshly-connected-camera
  SDK behaviour". They are absent here, and the environment finding below
  explains why.
- **`conformance`** — *"no errors, warnings or issues found"*, every member
  inside its response-time target: [conformance.log](conformance.log),
  machine-readable [conformance-results.json](conformance-results.json)
  (`ErrorCount`/`IssueCount`/`ConfigurationAlertCount`/`TimingIssuesCount`
  all 0). ConformU validated the new list directly — `ReadoutModes Read OK
  Raw16` / `OK Raw8`, `ReadoutMode Index OK … Current value: Raw16`,
  `MaxADU OK 65535` — and the first full frame transferred in 1.688 s.

## Readout-format contract (RM1-RM4), measured

Driven over Alpaca against the same running service:

| Check | Contract | Result |
|---|---|---|
| Advertised list | RM1 | `["Raw16", "Raw8"]` — the camera advertises both, so the ladder is real on this model |
| Default mode | RM1 | index 0 (`Raw16`), restored on every connect |
| `MaxADU` per mode | RM2 / ST3 | 65535 in `Raw16`, 255 in `Raw8` |
| 64 × 48 subframe, `Raw16` | RM2 | min 0, max 32267, mean 3274.3, 2253 distinct values |
| 64 × 48 subframe, `Raw8` | RM2 | min 0, max 160, mean 11.5, 71 distinct values — all within `MaxADU` |
| Same scene, both formats | RM2 | predicted `Raw8` mean 12.8 (= 3274.3 / 256) vs 11.5 observed; 8-bit truncation biases the observed value about half an LSB low |
| **Full frame in `Raw8`** | RM2 | 2976 × 3000, ImageBytes transmission element type 6 (`Byte`), payload **8 928 000 bytes = w × h × 1** — the `w × h` bytes the design doc asked for |
| Mid-exposure change | RM1 | rejected: `cannot change the readout mode while an exposure is in flight`, mode unchanged afterwards |
| Reconnect | RM1 | mode returns to 0 / `MaxADU` to 65535 |

A dark frame is **not** evidence for the 8-bit path: at unity gain this
camera's dark frame peaks around 115/65535, which truncates to a
legitimately all-zero `Raw8` frame. The measurements above were taken at
gain 600 so the scene clears one 8-bit LSB by a wide margin; gain and
offset were restored to 0 afterwards.

## Environment finding: the SDK needs a writable working directory

`SVBSetControlValue(SVB_GAIN)` fails with `SVB_ERROR_GENERAL_ERROR` —
surfaced as ASCOM `InvalidOperationException`, *"failed to set gain: SVBony
camera SDK error: general error"* — for every call between a connect and
that connection's first exposure, **whenever the process's working
directory is not writable by the user running it**. Once one frame has been
captured, gain sets succeed for the rest of the connection. `Offset`
(`SVB_BLACK_LEVEL`) is unaffected, and so is everything else.

Measured by toggling one variable on an otherwise identical transient unit
(same binary, same config, same user, same environment, same ~45 ms
connect-to-set gap):

| Working directory | `connect → set gain` |
|---|---|
| `/` (systemd-run default) | FAIL |
| `/home/<user>` (an ssh shell's cwd, mode 0700, other user) | FAIL |
| `/var/lib/rusty-photon` (the packaged unit's `WorkingDirectory`) | **OK** |

The mechanism is visible on disk: the SDK persists a per-model camera
configuration blob into the current directory — `U3SM900C-AST_Cfg_SAVE.bin`
appears in `/var/lib/rusty-photon` exactly when a gain write succeeds.

Consequences:

- **Production is unaffected**: every packaged systemd unit sets
  `WorkingDirectory=/var/lib/rusty-photon`, which the service user owns.
- It explains the four informational `PUT Gain` items in the prior
  SV605CC records (run by hand from a shell, not through the unit) and
  the design doc's "one unreproduced transient" `SVBSetControlValue`
  failure "shortly after the camera's very first connect".
- It is a live hazard for any future launch path that does **not** pin a
  writable working directory — a hand-run dev binary, a `systemd-run`
  without `--working-directory`, and in particular the Windows service
  packaging svbony-camera does not have yet (a Windows service's default
  cwd is `C:\Windows\System32`). Tracked as
  [#891](https://github.com/rusty-photon/rusty-photon/issues/891).

**Follow-up (2026-08-16, #891 resolved):** the working directory turned out
to be a proxy, not the cause. The SDK refuses `SVBSetControlValue(SVB_GAIN)`
while its auto-exposure state is on, which it is after every
`SVBOpenCamera`; the only SDK path that clears it is a manual
`SVB_EXPOSURE` write, which the driver used to issue only per exposure. A
writable cwd masked that because the SDK's default auto-save persisted the
previous session's "auto-exposure off" in `_Cfg_SAVE.bin` and reloaded it
at open. The connect handshake now mirrors `indi_svbony_ccd` (restore
defaults, auto-save off, manual `SVB_EXPOSURE`) — see the design doc's
C1a/GO5 and "Working directory (SDK-persisted camera config)". Confirmed on
this rig from a pristine working directory on 2026-08-16:
[2026-08-16-svbony-camera-sv605cc-rig-connect-handshake](../2026-08-16-svbony-camera-sv605cc-rig-connect-handshake/README.md).

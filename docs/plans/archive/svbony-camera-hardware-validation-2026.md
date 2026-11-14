# svbony-camera: the real-hardware validation passes, 2026-07 to 2026-08

A record, moved whole from `docs/services/svbony-camera.md` on
2026-11-13: the first Linux validation against a physical SV605CC
(2026-07-26), which predates the ConformU records in `docs/validation/`,
what it left open and what closed it, and the dated account of the passes
that followed on Windows and on the field rig. It stood under
[svbony-camera.md § Real-hardware validation](../../services/svbony-camera.md#real-hardware-validation);
that section lists the ConformU records in
[`docs/validation/`](../../validation/README.md) with what each settled,
and the sections this record names in quotes state what the service does
today.

## Real-hardware validation

**PASSED (2026-07-26)** against a physical SV605CC (hardware serial
`0123481353808C03EE2512150035`, USB VID `f266`) on a Linux x86_64 dev
machine, real SDK staged from the pinned indi-3rdparty blob
(`SVBONY_SDK_LIB_DIR`, sha256 matching `rusty-photon-svbony-sdk-install`'s
pinned hash). Headline: **ASCOM ConformU 4.3.0 passes both the
`alpacaprotocol` and full `conformance` suites with zero errors, zero
issues, and every member inside its response-time budget**, against the
production (non-simulation) binary serving the physical camera. (That
pass predated the `docs/validation/` record trail, so its ConformU
output was not preserved; a recorded Linux re-run on the same host and
camera — clean again, with the full ConformU output committed — lives in
[docs/validation/2026-07-27-svbony-camera-sv605cc-linux/](../../validation/2026-07-27-svbony-camera-sv605cc-linux/README.md).)

What ran, and what each open item resolved to:

- **Enumeration & identity** — the production build registers the real
  camera with `UniqueID` minted from the enumeration-time `CameraSN`
  (`SVBONY:SVBONY-SV605CC:<serial>`); `doctor` reports it. (The Phase E
  simulation-only registration boundary was removed as part of this
  validation — see "Device registration".)
- **`SVB_EXPOSURE` unit = microseconds, CONFIRMED** — a 3 s request
  integrates ~3.2 s wall-clock; SDK value quantization reads back at µs
  scale (200 000 → 199 997).
- **Stale-frame flush NOT needed on the soft-trigger path, CONFIRMED** —
  fresh frames with correct integration times immediately after
  ROI/exposure/bin changes; the aborted-frame case is closed by the abort
  path's stop-then-re-arm (Exposure contract, steps 3-4).
- **`CanStopExposure` stays `false`, CONFIRMED** — probed concurrently:
  `SVBStopVideoCapture` during a blocked `SVBGetVideoData` is tolerated
  (no crash; handle survives restart + capture) but does not unblock the
  read and the interrupted frame is never delivered — no data-preserving
  stop exists.
- **`ElectronsPerADU` permanently `NOT_IMPLEMENTED`, CONFIRMED** — no
  control type or exported function in SDK 1.13.4 exposes any conversion
  factor.
- **`MaxADU` corrected to 65535** — saturated Raw16 pixels read 65535 on
  the 14-bit sensor (full-range rescale, low bits populated), so the
  original `2^MaxBitDepth - 1` = 16383 understated the delivered scale.
- **`CameraXSize`/`CameraYSize` switched to R4 aligned-down reporting
  (2976×3000)** — real ConformU's binned full frame at bin 3 failed
  against the raw 3008 extent (1002 % 8 ≠ 0), the exact case Phase E
  deferred; every binned full frame (bins 1-4) is now BDD-pinned
  achievable and ConformU passes.
- **Abort made responsive** — measured drain to next accepted exposure
  ~0.3 s (was ~8.3 s on an aborted 10 s exposure), via the cancel flag
  checked between `SVBGetVideoData` poll slices + stop/re-arm; recovery
  exposures verified clean.
- **Gain/offset/cooling** — set/read round-trips against the real
  controls (gain 0-600, offset 0-100 on this camera); operator cooling
  cycle verified: TEC engages only on `CoolerOn` (tenet 3 honoured — the
  connect handshake was watched doing nothing to the cooler), power
  ramps (9→12 % toward a 20 °C setpoint from 28.7 °C ambient, sensor
  temperature falling), `CoolerOn=false` returns power to 0.
- **Full-frame transfer** — 3008×3008 Raw16 (~18 MB) downloads correctly;
  needs the udev rule's `usbfs_memory_mb` bump (the 16 MB usbfs default
  is below one full frame). Binned/ROI geometry byte counts verified
  exact at 640×480, 320×240, 1504×1504 (bin 2 full frame), 2976×3000.
- **SDK side effect discovered: parameter dumps in the working
  directory** — `libSVBCameraSDK` writes per-model auto-save config files
  (`U3SM900C-AST_Cfg_A.bin` / `_Cfg_SAVE.bin` for the SV605CC) into the
  process CWD (auto-save at close, `SVBRestoreDefaultParam` at connect).
  `.gitignore`d so dev-machine runs from a checkout can't commit them;
  the connect handshake now turns auto-save off (C1a), so only the
  `_Cfg_A.bin` write remains — see "Working directory (SDK-persisted
  camera config)".
- **One unreproduced transient** — a single `SVBSetControlValue(SVB_GAIN)`
  failure shortly after the camera's very first connect (immediately
  after the udev re-trigger), never seen again across >1000 subsequent
  Alpaca requests incl. two full ConformU runs. The seam previously
  discarded the SDK error detail, making it undiagnosable; error mapping
  now carries the SDK error text through every control/handshake path so
  any recurrence is attributable. **Explained**: the SDK rejects gain
  writes while its auto-exposure state is on, which it is after every
  open until a manual `SVB_EXPOSURE` write — the first-ever connect in a
  fresh checkout had no persisted `_Cfg_SAVE.bin` to mask it (GO5); the
  2026-08-05 rig A/B first attributed this to working-directory
  writability, and the connect handshake now clears the state itself
  (C1a) — see "Working directory (SDK-persisted camera config)".

Still open (not hardware-blockable on this camera):

- **Dark-frame banding revision check and a gain/offset sweep against
  advertised e-/ADU curves** (plan checklist) — needs a dark, temperature-
  controlled optical setup, deferred to field use; nothing in the driver
  depends on the outcome.
- **macOS Apple-Silicon SDK availability** — unchanged; no `mac_arm64`
  blob in indi-3rdparty as of SDK 1.13.4.

Closed since:

- **Connect handshake C1a on the physical camera** — closed 2026-08-16 by
  the rig pass described under "Field rig — connect handshake C1a"
  below: the packaged binary from a directory holding no `_Cfg_SAVE.bin`
  accepts `Gain` before any exposure, and ConformU `alpacaprotocol` +
  `conformance` are clean from there.

- **Packaged-binary RUNPATH end-to-end on a real target install** —
  closed by issue #704's fix: the Linux verifier (now `cargo ci
  verify-packages`) installs
  the package in a systemd container, runs the shipped
  `rusty-photon-svbony-sdk-install`, starts the unit and `ldd`-proves the
  binary resolves the operator-installed blob from
  `/usr/lib/rusty-photon`, on both the deb and rpm flavors, on every
  nightly run. The "on the rig, against the physical camera" half closed
  2026-07-30 — see "Field rig" below.
- **Dev-machine USB permissions** — closed by issue #710's fix: the
  helper installs a dev udev rule on hosts with no packaged one (see
  "udev / USB"), so the hand-written rule this validation needed is now
  part of the same bootstrap step as the SDK.

### Windows (2026-07-26)

The same physical SV605CC passed the full validation on Windows 11
(25H2): ConformU 4.4.0 `alpacaprotocol` and `conformance` both clean
against the real-SDK release binary at commit `ef03a1cd`, `UniqueID`
minting identical to Linux, and every sharp path above re-confirmed —
abort→next-accepted-exposure ~0.08 s, the ~18 MB full frame transfers
byte-exact with **no analogue of the `usbfs_memory_mb` bump needed**,
and the cooler engages only on `CoolerOn`. Windows-specific ground
truth: the camera carries no Microsoft OS descriptors, so WinUSB never
auto-binds — SVBony's driver package (INF v1.0.0.8) is a hard
prerequisite, and until it is installed the SDK reports zero devices
(the same silent shape as the Linux `errno=13` case above); and the
SDK's parameter dumps land in `%APPDATA%\CKConfig\`, not the process
CWD (the auto-exposure gain gate GO5 itself is platform-independent and
is cleared by the connect handshake, C1a, on every platform). Full
environment details and the unmodified ConformU output:
[docs/validation/2026-07-26-svbony-camera-sv605cc-windows/](../../validation/2026-07-26-svbony-camera-sv605cc-windows/README.md).
Operator setup: [docs/svbony-camera-windows-install.md](../../svbony-camera-windows-install.md).

### Field rig (2026-07-30)

The same physical SV605CC passed both ConformU suites on the Pi 5
telescope field rig (aarch64, Raspberry Pi OS) — this time against the
**packaged** binary: the arm64 nightly deb installed from the apt repo,
the SDK provisioned by the shipped `rusty-photon-svbony-sdk-install`
(the documented retry-until-SDK-lands startup observed live), and the
packaged udev rule providing device access and the `usbfs_memory_mb`
bump. ConformU 4.4.0 `alpacaprotocol` and `conformance` both clean over
the LAN, with the identical four informational `PUT Gain` casing items
as every prior platform, full-frame transfers at bins 1–4, and identical
`UniqueID` minting. Full environment details and the unmodified ConformU
output:
[docs/validation/2026-07-30-svbony-camera-sv605cc-rig/](../../validation/2026-07-30-svbony-camera-sv605cc-rig/README.md).

### Field rig — readout formats (2026-08-05)

The readout-format ladder (issue #882) ran against the same physical
SV605CC on the rig at commit `4b8b8179`, closing the "no real frame has
come off the hardware in 8-bit" item this section carried when the
feature merged. The camera advertises **both** formats, so
`ReadoutModes` is `["Raw16", "Raw8"]` and the ladder is real on this
model rather than a one-entry list. Measured: `MaxADU` 65535 / 255 per
mode; a **full 2976 × 3000 frame downloaded in `Raw8`** as ImageBytes
transmission element type 6 (`Byte`) at exactly 8 928 000 bytes = `w × h`;
the same scene in both formats consistent to within 8-bit truncation
(`Raw16` mean 3274.3 → predicted `Raw8` 12.8, observed 11.5); a
mid-exposure `ReadoutMode` change rejected with the mode left unchanged;
and the mode reset to `Raw16` on reconnect. ConformU 4.4.0 clean on both
suites — and `alpacaprotocol` with **zero information alerts**, where
every prior SV605CC record carries four informational `PUT Gain` items;
those turned out to be the SDK's auto-exposure gain gate (GO5), which
that run's writable working directory happened to mask (see "Working
directory (SDK-persisted camera config)") — not the camera. A dark
frame proves nothing here (at unity gain this sensor's dark peak
truncates to an all-zero `Raw8` frame, correctly), so the frames were
taken at gain 600. Record:
[docs/validation/2026-08-05-svbony-camera-sv605cc-rig-readout/](../../validation/2026-08-05-svbony-camera-sv605cc-rig-readout/README.md).

### Field rig — production TLS+auth endpoint, ConformU 4.5.0 (2026-08-08)

Routine re-validation of the packaged nightly at commit `c530940a`, and
the first ConformU run made against the service's production TLS + HTTP
Basic auth endpoint rather than a plain-HTTP or tunnelled loopback one:
certificate chain verified against the system trust store, every request
authenticated, no configuration changes on the rig. Both suites clean on
ConformU 4.5.0 (first SV605CC record on that version), `alpacaprotocol`
again with zero information alerts. The record also documents two
ConformU behaviours anyone repeating this needs: the `conformance` suite
takes its URL scheme from `AlpacaConfiguration.AccessServiceType` rather
than the CLI URI (leave it `Http` and the 308 redirect plus .NET's
header-stripping on cross-scheme redirects turns correct credentials
into `Unauthorized`), and the `alpacaprotocol` suite's embedded ASCOM
client-library calls carry no credentials at all — an upstream gap
worked around with a header-injecting loopback proxy. Record:
[docs/validation/2026-08-08-svbony-camera-sv605cc-rig/](../../validation/2026-08-08-svbony-camera-sv605cc-rig/README.md).

### Field rig — connect handshake C1a (2026-08-16)

The pass that closed the C1a / GO5 open item after the issue #891 fix
shipped in the packaged nightly at commit `97854524`. The installed
binary was started from a working directory holding nothing but its
config file — no `U3SM900C-AST_Cfg_SAVE.bin` from any earlier session, the
exact scenario the issue describes — and accepted `PUT Gain` immediately
after connect; the previously deployed binary, run through the identical
procedure, still answered *"failed to set gain: … general error"* until
its first exposure. Both ConformU suites are clean from that pristine
directory on 4.5.0, `alpacaprotocol` with zero information alerts, which
settles that the historical `PUT Gain` items were the auto-exposure gate
and not the working directory. The SDK wrote its cfg-file pair once at
connect (the restore) and never at close (auto-save off). The run also
found the `SDK_LOG=yes` close-time segfault documented under "Working
directory (SDK-persisted camera config)". Record:
[docs/validation/2026-08-16-svbony-camera-sv605cc-rig-connect-handshake/](../../validation/2026-08-16-svbony-camera-sv605cc-rig-connect-handshake/README.md).

### Field rig — reconnect during an exposure, E10 (2026-08-23)

The pass for the E10 half of issue #889, taken on the branch of PR #1062
(commit `74aa87e6`) rather than on a packaged nightly: the driver built
from source on the rig with default features and run against the physical
SV605CC. Both ConformU suites clean on 4.5.0 — `alpacaprotocol` with zero
information alerts, `conformance` with zero errors, issues, configuration
alerts and timing issues across 83 timed members — and every property the
camera reports identical to the 2026-08-08 record, which is the point for a
change that only rewires how a capture is cancelled. A purpose-built
harness put the rewired paths on the hardware: 13/13, including
`AbortExposure` reaching `Idle` in 0.01 s with the device handed back
0.11 s later (one poll slice), disconnect cancelling an in-flight exposure
in 0.12 s, and an E10 sweep of twelve disconnect points spanning
integration, readout and past completion at 12/12, then 30/30 at the
integration/readout boundary.

The same harness was run against the pre-PR packaged binary for an A/B, and
passed identically — the race does not reproduce through the Alpaca API on
a healthy box, nor with the service pinned to one core crowded by 64
spinners (load average 65), which stretched every measured wall time by
~70% without opening the window. The reason is structural and is recorded
under "Concurrency": a capture holds the camera lock across its
`SVBGetVideoData` slice and `set_connected(false)` sets the cancel flag
before taking that lock, so the superseded capture has to be preempted in
the sliver between releasing the lock and reading the flag, and stay
preempted for the whole reconnect. The deterministic proof stays in the
unit tests, each verified to fail against the pre-fix shape. The run also
surfaced the unrelated first-read deadline gap
([#1067](https://github.com/rusty-photon/rusty-photon/issues/1067)),
reproduced on both binaries and since closed by the read-deadline floor in
the exposure contract, step 2d. Record:
[docs/validation/2026-08-23-svbony-camera-sv605cc-rig-reconnect/](../../validation/2026-08-23-svbony-camera-sv605cc-rig-reconnect/README.md).

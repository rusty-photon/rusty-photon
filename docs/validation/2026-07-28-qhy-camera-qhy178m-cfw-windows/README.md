# qhy-camera on Windows — QHY178M + CFW, 2026-07-28

Recorded Windows ConformU run against the **same physical QHY178M and CFW** as
the [Linux record](../2026-07-28-qhy-camera-qhy178m-cfw-linux/README.md) taken
the same day, at the same commit. First real-hardware Windows validation for
this service.

## What was tested

| | |
|---|---|
| Commit | [`7d9d93c9`](https://github.com/rusty-photon/rusty-photon/commit/7d9d93c9) — branch `fix/qhy-cfw-position-cache`, i.e. `origin/main` (`b2c73436`) plus the CFW position-cache fix |
| Service | `qhy-camera`, **real-SDK** build (default features, no `QHYCCD_SKIP_NATIVE_LINK`), delay-loaded `qhyccd.dll` per WD1 |
| Build | `cargo build --release -p qhy-camera` with `QHYCCD_SDK_DIR` set to the SDK's `x64` dir and `LIBCLANG_PATH` for bindgen; rustc 1.97.1 |
| SDK | QHYCCD SDK **26.06.04** — `sdk_win64_26.06.04\x64\qhyccd.dll`, file version `26, 6, 4, 16`, sha256 `c7cea0039c3719388dcbb38f02524d4bdc6aaa827495056a2ec3b5bb24551d5f`; the pinned version, on `PATH` so the delay-load resolves it rather than the All-in-One's older copy |
| Driver | QHY **All-in-One 25.06.16** (`QHYCCD_Win_AllInOne.25.06.16.16.exe`) — supplies the signed `qhycameras.inf` (25.6.13.908, class `AstroCams`, "Microsoft Windows Hardware Compatibility Publisher"). The documented operator path per [ADR-015](../../decisions/015-windows-packaging-architecture.md) decision 6 |
| Platform | Windows 11 Pro 25H2 x64, in a KVM guest; camera passed through on `qemu-xhci` |
| Camera | QHY178M, 3056×2048, mono, `MaxADU` 65535 — SDK id `QHY178M-222b16468c5966524` |
| FilterWheel | The CFW on that camera's port, 7 slots, same physical `OpenQHYCCD` handle |
| ConformU | **4.4.0** build 52526, run inside the guest against `http://127.0.0.1:11121/api/v1/camera/0` and `.../filterwheel/0` |

## Verdicts

Both devices, both suites, clean:

| Device | `alpacaprotocol` | `conformance` |
|---|---|---|
| Camera | 0 errors, 0 issues, 16 information messages — [log](alpacaprotocol-camera.log) | *"no errors, warnings or issues found"*, all 70 timed members within target — [log](conformance-camera.log), [results](conformance-camera-results.json) |
| FilterWheel | *"no errors, issues or information alerts"* — [log](alpacaprotocol-filterwheel.log) | *"no errors, warnings or issues found"*, all 33 timed members within target — [log](conformance-filterwheel.log), [results](conformance-filterwheel-results.json) |

`ErrorCount` / `IssueCount` / `ConfigurationAlertCount` / `TimingIssuesCount`
are **0** in both results files.

The camera's 16 informational items are identical to the Linux run — the
protocol suite's casing probes against `ImageArray` / `LastExposureDuration` /
`LastExposureStartTime` before any exposure exists, answered with in-protocol
ASCOM errors over HTTP 200. **Cross-platform parity holds**: same verdicts, same
informational set, and the same `UniqueID`s as Linux
(`QHY178M-222b16468c5966524`, `CFW-QHY178M-222b16468c5966524`).

The FilterWheel's clean timing result also confirms the CFW position cache on
Windows: without it `Position` and `DeviceState` exceed ConformU's 100 ms FAST
target on this hardware (see the Linux record).

## Windows provisioning notes

Things this run had to establish, none of them obvious from the docs:

- **The All-in-One's nested driver installer needs its own silent flags.**
  Running the outer `QHYCCD_Win_AllInOne` exe with `/VERYSILENT` installs the
  SDK but leaves the nested `QHYCamerasDriver-…-NewIO.exe` sitting on a dialog
  that never appears over an SSH session. Invoking that inner installer directly
  with `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-` completes it (exit 0).
- **A staged driver does not bind on its own.** After the driver package is in
  the store (`pnputil /enum-drivers` shows `qhycameras.inf`), the camera still
  reported `Status: Error` with no class until it was **re-plugged**; then it
  came up `Status OK, Class AstroCams`.
- **Two `qhyccd.dll`s exist after an All-in-One install** — the pack's own
  (`C:\Program Files\QHYCCD\AllInOne\sdk\x64`) and whichever SDK the binary was
  built against. This run put the pinned 26.06.04 `x64` directory first on
  `PATH` so the delay-load binds the version the build expects.
- **A wedged camera survives a service restart** — and the wedge is *not*
  specific to Windows or to provisioning. During an earlier attempt the device
  stopped enumerating (`cameras=0`) and stayed that way across service restarts,
  a `doctor` invocation, and re-plugging via the hypervisor; only a physical
  power cycle cleared it, after which the same binary at the same commit
  produced this clean run. The same failure has since been reproduced on Linux
  (cascading `Test abandoned … camera is in state: Exposing`, then `Connected`
  and the FilterWheel's connect exceeding their 5 s timeouts), so it is a
  recurring hardware/SDK-level state, not an artifact of the driver install —
  tracked in
  [issue #755](https://github.com/rusty-photon/rusty-photon/issues/755). Both
  records here were taken on a healthy, freshly power-cycled camera; if a run
  starts producing that pattern, power-cycle before suspecting the exposure
  path.

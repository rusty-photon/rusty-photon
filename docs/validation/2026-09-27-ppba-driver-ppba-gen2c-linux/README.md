# ppba-driver on Linux — PPBADV Gen2C, 2026-09-27 (first hardware record)

The first recorded ConformU run of `ppba-driver` against a physical Pocket
Powerbox Advance. It was taken right after the Total Current fix
([#1346](https://github.com/rusty-photon/rusty-photon/pull/1346)): `PA`'s
current is a raw sense count, and the driver used to publish it unscaled,
outside the switch's own published range. Both devices were run through both
suites, with **auto-dew off**, the documented procedure (see
[the precondition](#the-auto-dew-precondition) below — a run in the box's
normal auto-dew-on state produced a finding, now
[#1347](https://github.com/rusty-photon/rusty-photon/issues/1347)).

## What was tested

| | |
|---|---|
| Commit | [`4b9d0a26`](https://github.com/rusty-photon/rusty-photon/commit/4b9d0a26) — `main` at the #1346 merge |
| Service | `ppba-driver`, default features (**no** `mock`), started with `cargo run -p ppba-driver -- -c <config> -l debug` |
| Build | dev profile; rustc 1.98.1 (48a229cea 2026-09-01) |
| Platform | Fedora Linux 44 (Workstation Edition) x86_64, kernel 7.2.5-200.fc44 |
| Device | Pegasus Astro **PPBADV Gen2C**, USB serial `PPBA9V2RNC` (FTDI `0403:6015`), firmware **2.12.3** (`PV`), on `/dev/serial/by-id/usb-Pegasus_Astro_PPBADV_Gen2C_PPBA9V2RNC-if00-port0` |
| `UniqueID`s | minted on first run of this config: Switch `3cb8384e-98c8-44d3-8c15-6184ab29b774`, ObservingConditions `298ccc80-858f-492a-a245-8960b5ca8740` |
| Server | `127.0.0.1:11112`, plain HTTP, no auth |
| ConformU | **4.5.0** build 53834.49ab847 (= latest release at run time), default hardware timing: `SwitchReadDelay` 500 ms, `SwitchWriteDelay` 3000 ms |

## Verdicts

Both devices, both suites, clean:

| Device | `alpacaprotocol` | `conformance` |
|---|---|---|
| Switch | *"no errors, issues or information alerts"* — [log](alpacaprotocol-switch.log) | *"no errors, warnings or issues found"*, 167 timed members, all within target — [log](conformance-switch.log), [results](conformance-switch-results.json) |
| ObservingConditions | *"no errors, issues or information alerts"* — [log](alpacaprotocol-observingconditions.log) | *"no errors, warnings or issues found"*, 21 timed members, all within target — [log](conformance-observingconditions.log), [results](conformance-observingconditions-results.json) |

`ErrorCount` / `IssueCount` / `ConfigurationAlertCount` / `TimingIssuesCount`
are **0** in both results files.

The `conformance` logs' `INFO` lines are the expected ones:

- **Switch:** the `SetSwitchValue` offset tests note that a value set between
  PWM steps reads back rounded to a whole step, which is `PwmDuty`'s rounding.
  Async tests are skipped because the device has no asynchronous switches.
- **ObservingConditions:** `DeviceState` omits the ten sensors the PPBA does
  not have (cloud cover, pressure, rain rate, sky brightness, quality and
  temperature, star FWHM, and wind direction, gust and speed). The members
  themselves return `NOT_IMPLEMENTED`.

## What this pins

- **Total Current is inside its published range on hardware.** In
  `DeviceState`, switch 11 reads `0.2153846…` A, which is the box's count of 14
  divided by 65, inside `[0, 20]`. The same count used to be published as
  14.0 A, and a count of 40 as 40 A.
- **The full write path, on every writable switch.** ConformU drove the quad
  12 V output, the adjustable output, the USB2 hub and auto-dew through their
  boolean tests. It set both dew heaters at eleven points across 0-255, each
  read back exactly, and probed the offsets between PWM steps. The ten
  read-only switches refused writes as `NOT_IMPLEMENTED`. Every write was
  echoed and read back with no transport
  fault: the service log for the recorded run carries **no** timeout,
  communication, parse or `INVALID_OPERATION` line in 1680 lines. Its `ERROR`
  lines are all the `INVALID_VALUE` refusals ConformU provokes deliberately
  (switch ids −1 and 17, an unknown sensor name).
- **Outputs are restored.** State read over Alpaca before and after the
  session was identical for every writable switch: quad off, adjustable on,
  auto-dew on (set back after the run), USB hub 0. Dew duty is under auto-dew
  control in both snapshots. The USB hub state comes from the driver's shadow,
  because `PA` does not report it. The hub is the box's USB2 sub-hub, which had
  nothing attached. The cameras on the box's USB3 hub were idle, and their
  12 V comes from the quad output, which was off before and after.

## The auto-dew precondition

The design doc tells an operator to turn auto-dew off before a hardware
ConformU run, and this record followed it. A first Switch `conformance` run in
the box's normal state, **auto-dew on**, reported `0 errors / 4 issues / 0
alerts / 0 timing issues`, all of the same kind. `CanWrite` for dew heaters A
and B correctly reads false while auto-dew runs them. But `SetSwitch` /
`SetSwitchValue` on those switches answer `INVALID_OPERATION`, where ASCOM
requires `NOT_IMPLEMENTED` from a switch whose `CanWrite` is false.
`upbv2-driver` already answers `NOT_IMPLEMENTED` in the same situation.

That run is not recorded here; failures belong in issues. It is
[#1347](https://github.com/rusty-photon/rusty-photon/issues/1347). Once #1347
is fixed, the precondition goes away, and the next record should be taken with
auto-dew **on**, the state the box normally runs in.

# ppba-driver on Linux — PPBADV Gen2C with auto-dew on, 2026-09-27

The first `ppba-driver` record taken with **auto-dew on**, the state a Pocket
Powerbox Advance normally runs in. The service's
[first record](../2026-09-27-ppba-driver-ppba-gen2c-linux/README.md), taken
earlier the same day, had to turn auto-dew off. With it on, a write to a dew
heater answered `INVALID_OPERATION` while the heater reported
`CanWrite = false`: four ConformU issues
([#1347](https://github.com/rusty-photon/rusty-photon/issues/1347)). This run
tests the fix on the same box, with all four suites.

## What was tested

| | |
|---|---|
| Commit | [`686848f2`](https://github.com/rusty-photon/rusty-photon/commit/686848f2) — branch `fix/ppba-auto-dew-write-not-implemented`: `AutoDewEnabled` answers `NOT_IMPLEMENTED` |
| Service | `ppba-driver`, default features (**no** `mock`), started with `cargo run -p ppba-driver -- -c <config>` |
| Build | dev profile; rustc 1.98.1 (48a229cea 2026-09-01) |
| Platform | Fedora Linux 44 (Workstation Edition) x86_64, kernel 7.2.5-200.fc44 |
| Device | Pegasus Astro **PPBADV Gen2C**, USB serial `PPBA9V2RNC` (FTDI `0403:6015`), firmware **2.12.3**, on `/dev/serial/by-id/usb-Pegasus_Astro_PPBADV_Gen2C_PPBA9V2RNC-if00-port0` |
| `UniqueID`s | the same config as the first record: Switch `3cb8384e-98c8-44d3-8c15-6184ab29b774`, ObservingConditions `298ccc80-858f-492a-a245-8960b5ca8740` |
| Server | `127.0.0.1:11112`, plain HTTP, no auth |
| ConformU | **4.5.0** build 53834.49ab847 (= latest release at run time), default hardware timing: `SwitchReadDelay` 500 ms, `SwitchWriteDelay` 3000 ms |
| Starting state | auto-dew **on** (switch 5 = 1), dew heaters A/B at 26 under auto-dew and reporting `CanWrite = false`; quad 12 V off, adjustable output on, USB hub 0 |

## Verdicts

Both devices, both suites, clean:

| Device | `alpacaprotocol` | `conformance` |
|---|---|---|
| Switch | *"no errors, issues or information alerts"* — [log](alpacaprotocol-switch.log) | *"no errors, warnings or issues found"*, 163 timed members, all within target — [log](conformance-switch.log), [results](conformance-switch-results.json) |
| ObservingConditions | *"no errors, issues or information alerts"* — [log](alpacaprotocol-observingconditions.log) | *"no errors, warnings or issues found"*, 21 timed members, all within target — [log](conformance-observingconditions.log), [results](conformance-observingconditions-results.json) |

`ErrorCount` / `IssueCount` / `ConfigurationAlertCount` / `TimingIssuesCount`
are **0** in both results files.

The `conformance` logs' `INFO` lines are the same expected ones as in the
first record: PWM-step rounding in the `SetSwitchValue` offset tests, async
tests skipped (no asynchronous switches), and `DeviceState` omitting the ten
sensors the PPBA does not have. On the out-of-range writes, ConformU prints
*"Switch threw an InvalidOperationException when a value below
SwitchMinimum was set"*. That is fixed text in ConformU's
`HandleInvalidValueExceptionAsOk` branch. The service log shows the driver
answered `INVALID_VALUE`.

## What this pins

- **The auto-dew refusal is the one ASCOM requires.** ConformU read
  `CanWrite: False` for dew heaters A and B, and logged *"CanWrite is False
  and a NotImplementedException was thrown"* for both `SetSwitch` and
  `SetSwitchValue` on each (`conformance-switch.log` lines 200-224). These are
  the four checks that were issues before the fix. The ten read-only sensor
  switches answer the same way.
- **Auto-dew itself is still fully writable.** Switch 5 went through its
  boolean, min/max and offset tests with auto-dew as the switch under test.
  ConformU then reset it to the value it found, `1`. ConformU tests switches in
  ascending order and reads each `CanWrite` once, so the heaters were judged in
  the auto-dew-on state before switch 5 was touched.
- **Clean transport.** The service log (435 lines) carries no timeout,
  communication, parse or `INVALID_OPERATION` line. Its `ERROR` lines are all
  the `INVALID_VALUE` refusals ConformU provokes deliberately: switch ids
  below 0 and above 15, values of −1 and 2 on the boolean switches, an
  unknown sensor name, and an averaging period of −2 h.
- **Total Current stays scaled.** `DeviceState` switch 11 reads `0.2153846…` A
  (a count of 14 ÷ 65), as in the first record.
- **Outputs left as found.** State read over Alpaca before and after the
  session was identical for every writable switch: quad off, adjustable on,
  USB hub 0, auto-dew on, heaters at 26 under auto-dew control.

## Compared with the auto-dew-off record

The two records cover complementary halves of the heater contract:
- **Auto-dew off:** the heaters' write path, eleven PWM points across 0-255,
  each read back.
- **Auto-dew on:** the heaters' refusal.

The Switch `conformance` suite took 3 minutes here against about 9 there. With
`CanWrite` false, the heaters' writes throw at once and never reach their
3000 ms write delays. The four timed members this run lacks (163 against 167)
are exactly `SetSwitch` and `SetSwitchValue` on switches 2 and 3.

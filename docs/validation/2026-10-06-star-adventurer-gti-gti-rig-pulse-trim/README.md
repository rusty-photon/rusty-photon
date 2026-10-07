# star-adventurer-gti RA pulse edge-step trim on the field rig — Star Adventurer GTi, 2026-10-06

The ConformU run for the RA pulse edge-step trim defaults that #1406 shipped
(`mount.ra_pulse_edge_steps` 1.5 / 1.5 ticks, midway between the motor
board's two edge-step levels; see the design doc's "What the rig measured"
under §PulseGuide lifecycle). It runs the packaged nightly with **no rig
override**, so the East/West legs measure the shipped defaults. ConformU
pulses at mechanical HA −9 h and −3 h, where the board sits at its low and
its high level respectively, so the run covers the trim at both.

**This is a scoped record, filed by decision.** The
[all-zero rule](../../skills/hardware-validation.md) is not met: `conformance`
reports **1 issue**. It is not in the behaviour under test. All eight
East/West legs pass. The issue is an RA *reading* taken during a Dec pulse,
the cross-axis read noise that the open issue
[#1371](https://github.com/rusty-photon/rusty-photon/issues/1371) carries.
The decision to file is recorded there:
[#1371 comment](https://github.com/rusty-photon/rusty-photon/issues/1371#issuecomment-6032439259).
A clean two-suite record is still owed. It needs #1371's multi-sample
`RightAscension` read to land first.

## What was tested

| | |
|---|---|
| Commit | [`7700ee97`](https://github.com/rusty-photon/rusty-photon/commit/7700ee97) (`main`). It includes the trim defaults (#1406, `6f1b70d6`) and the design-doc record of the edge-step levels (#1415) |
| Service | **Package:** arm64 nightly deb `rusty-photon-star-adventurer-gti_0.1.0+nightly.202610061157.g7700ee9_arm64.deb`.<br>**Built by:** `nightly-packages` run [37459834180](https://github.com/rusty-photon/rusty-photon/actions/runs/37459834180).<br>**sha256:** `26b69d95ceb96366bbde00d0bb65629b6f8cd225d5b17492f46d0a5daa4018ad`.<br>**Installed over:** `0.1.0+nightly.202610041846.g7d6f73b`.<br>All 18 rusty-photon packages were upgraded to the same nightly. Not a source build |
| Platform | Raspberry Pi 5 Model B Rev 1.1 (aarch64), running Raspberry Pi OS (Debian 13 trixie), kernel `6.18.34+rpt-rpi-2712`. This is the telescope field rig |
| Mount | Sky-Watcher Star Adventurer GTi.<br>**Connection:** USB, through the motor board's STM32 virtual COM port (`/dev/serial/by-id/usb-STMicroelectronics_STM32_Virtual_ComPort_4E8741795300-if00`), at 115200 baud with a `polling_interval` of 200 ms.<br>**ASCOM `UniqueID`:** `b7afdd2e-851a-4b82-9e9a-2258ff652821`, the identity the driver minted into `mount.unique_id` on this install |
| Mount config | **No `ra_pulse_edge_steps` key**, so the defaults are in force; this is the point of the run. The rig's earlier West 1.75 override was removed before the run.<br>**Other settings:**<br>• `flip_policy.enabled = true`, `auto_flip_during_tracking = false`<br>• `cw_exclusion_zone` at the shipped default (0.95, 11.05 h)<br>• `site_latitude_deg = 32.7157`<br>• `min_altitude_degrees = 0`<br>• `unpark_from_ap_position = ap_park_3`<br>• `settle_after_slew = 2s`<br>• Park pair −907200 / 725760 ticks |
| Logging | `RUST_LOG=info,rusty_photon_shared_transport=trace`. The wire trace is what the offline replay below reads |
| Transport to the device | **`conformance`:** over the service's production HTTPS + HTTP Basic endpoint.<br>**`alpacaprotocol`:** through the loopback header-injecting proxy of the [2026-09-26 record](../2026-09-26-star-adventurer-gti-gti-rig/README.md), whose README explains why |
| ConformU | **Version:** 4.5.0 build 53834 (`linux-arm64`). This is the latest release at run time.<br>**Where it ran:** on the rig itself, as the transient systemd unit `cu1406`.<br>**Invocation:** `conformu conformance <url> -s conformance.settings -n conformance.log -r conformance-results.json`, plus the `alpacaprotocol` verb through the proxy. **Every test group ran**, because both verbs call `SetFullTest()`.<br>**Settings file:** connection settings (`Https` and the observatory credential, `StrictCasing`) and tolerances at ConformU's defaults: 1″ pulse-guide and 10″ slew, printed inline as 1″ in Dec and 0.07 s in RA |
| Operations | `rp` was stopped for the run and restarted afterwards. The service was idle for 120 s before ConformU started.<br>**Start state:** parked at the Park 3 pose, tracking off.<br>**Motion:** the full test set includes the `SideOfPier Write` meridian flip |

Log timestamps are the rig's local time, Pacific Daylight Time (UTC−7):

| Suite | Local time (2026-10-06) | UTC (2026-10-07) |
|---|---|---|
| `alpacaprotocol` | 23:05:11–23:07:30 | 06:05:11–06:07:30 |
| `conformance` | 23:07:30–23:24:58 | 06:07:30–06:24:58 |

ConformU's `TimeCheck` names the zone by its standard offset (UTC−08:00), the
way .NET labels a time zone. The `PC UTCDate` it logs (06:07:32 against local
23:07:32) shows the offset actually in force.

## Verdicts

- **`alpacaprotocol`:** *"Found 0 errors, 0 issues and 3 information
  messages"* ([alpacaprotocol.log](alpacaprotocol.log)).
  - The three information items are the `PUT Park` ID-casing variants answered
    with `InvalidOperationException: park refused: slew already in progress`.
  - They are the same three every rig run records.
- **`conformance`:** **0 errors, 1 issue, 0 configuration alerts, 0 timing
  issues** ([conformance.log](conformance.log), machine-readable
  [conformance-results.json](conformance-results.json)).
  - All 93 timed members stayed inside their target.

## The thing under test: East/West pulses with the default trim

ConformU sends 5 s pulses at the default guide rate (0.5 × sidereal). It
expects ±2.51 s of RA, with a tolerance of 0.07 s.

| Leg | Mechanical HA | Edge-step level there | East | West |
|---|---|---|---|---|
| HA −9 | −9 h | low | +2.54 s (diff 0.03) | −2.50 s (diff 0.01) |
| HA +9 | −3 h (through the pole) | high | +2.52 s (diff 0.01) | −2.53 s (diff 0.01) |
| HA −3 | −3 h | high | +2.50 s (diff 0.02) | −2.53 s (diff 0.01) |
| HA +3 | −9 h (through the pole) | low | +2.52 s (diff 0.01) | −2.48 s (diff 0.03) |

- **Every East/West leg passed at both levels.** The largest difference is
  0.03 s, under half the tolerance.
- **Compared with the old defaults:** the 2026-10-03 rig run on the old
  defaults (1.38 / 3.23) failed two West legs, at HA −9 and +3. Those are the
  low-level poses, where the old West trim cut the pulse short.

## The one issue: an RA reading during a Dec pulse — #1371

`PulseGuide +3.0 North`: *"East-West movement was outside test tolerance: RA
change (HMS): +00:00:00.10, Expected: +00:00:00.00, Tolerance:
+00:00:00.07"*. The Dec motion itself was right: +37.9″ against +37.6″.

**It is read noise, not RA motion.** An offline replay of this run's wire
trace shows this:
- **What the replay does:** it rebuilds the samples the driver published
  from the TRACE log and evaluates `RightAscension` at ConformU's own read
  instants.
- **With the driver's read:** a single `:j1` sample, projected to the read
  instant. The replay reproduces the failure, at +0.073 s against the printed
  +0.10 s.
- **With a 5- or 10-sample fixed-slope read at the same instants:** +0.020 to
  +0.025 s, inside the tolerance.

That is the failure #1371 describes. Its remaining direction is a
multi-sample `RightAscension` read. It is not the trim (a Dec pulse never
changes the RA rate), and not #1406.

Single-sample read noise has failed a leg in earlier rig runs too, but not
in every run:
- **2026-10-04:** one leg, the HA −9 East magnitude.
- **2026-10-05:** no leg, on the previous nightly with a rig-only West 1.75
  override. That run was all-zero, but it validated an override, not the
  shipped defaults, so it was not filed.

## Also on record from this run

- **Dec pulses:** all eight moved the right way, within 0.8″ of ±37.6″, on
  both pointing states. These are the counterweight-up legs at HA +3 and +9
  that the 2026-09-26 record first measured.
- **Meridian flip:** `SideOfPier Write` tracked through the meridian for seven
  minutes and then *"Successfully flipped pierWest to pierEast"*.
- **Pointing state:** `SideofPier` and `DestinationSideofPier` read `WE` at
  HA ±9 and ±3.
- **Park:** `Park` took 16 s to the Park 3 pair. `Unpark`, the parked
  refusals and `AbortSlew` (stopped at once) are all OK.
- **`SetPark`:** it rewrote the same park pair into the config.

## Files

- [`alpacaprotocol.log`](alpacaprotocol.log): unmodified in content. The
  device under test appears as the loopback proxy (`127.0.0.1:18117`).
  ConformU wrote six CR characters inside its multi-line `Response:`
  entries; the repository's `.gitattributes` stores every file with LF line
  endings, so those CRs are not in the committed copy.
- [`conformance.log`](conformance.log): unmodified **except lines 5 and 7**.
  There the rig's hostname is replaced by the placeholder `<rig-host>`,
  because the rig's address must not appear in this public repository.
- [`conformance-results.json`](conformance-results.json): unmodified. It
  carries no host.

## State the rig was left in

- **Mount:** parked on the Park 3 tick pair by the kit's cleanup, tracking
  off.
- **Clients:** `rp` restarted and holding the mount connection again.
- **Config:** the GTi config is the 2026-10-04 file, without
  `ra_pulse_edge_steps`. `SetPark`'s rewrite left its content unchanged.
- **Packages:** all 18 rusty-photon packages at nightly `g7700ee9`.

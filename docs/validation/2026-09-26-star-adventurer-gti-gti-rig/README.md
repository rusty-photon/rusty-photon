# star-adventurer-gti counterweight-up PulseGuide on the field rig — Star Adventurer GTi, 2026-09-26

The first `docs/validation/` record for `star-adventurer-gti`, made to close
the obligation issue [#1332](https://github.com/rusty-photon/rusty-photon/issues/1332)
left open after [#1300](https://github.com/rusty-photon/rusty-photon/issues/1300)
(PR #1312, merged as `6eaa9b70`): Dec guide pulses now resolve `ccw` against
the pier side the mount is on, so `guideNorth` moves the OTA north on the
counterweight-up side too. That fix had been verified against the mock and
*derived* for the counterweight-up side from counterweight-down hardware runs;
this run **measures** it there. ConformU's extended PulseGuide tests reached
HA +3 and +9 with the mount through the pole and read North/South the right
way, at the right magnitude.

**This is a scoped record, filed by decision.** The
[all-zero rule](../../skills/hardware-validation.md) is not met: `conformance`
reports 11 issues. All eleven are on the RA axis — seven are the known
[#1299](https://github.com/rusty-photon/rusty-photon/issues/1299) East/West
stop-window offset, four are a cross-axis RA *reading* jitter filed from this
run as [#1334](https://github.com/rusty-photon/rusty-photon/issues/1334) — and
none touches Dec. Of the two options #1332 offered (file scoped now, or wait
for #1299 and file one clean run), this takes the first: the #1300 evidence is
worth having now and #1299 has no timeline. A clean two-suite record is still
owed, and it needs **both** RA defects fixed first — #1299 accounts for seven
of the eleven issues and #1334 for the other four, so landing either one alone
still leaves a nonzero run.

## What was tested

| | |
|---|---|
| Commit | [`6eaa9b70`](https://github.com/rusty-photon/rusty-photon/commit/6eaa9b70) (`main`; the #1300 fix, merge of PR #1312) |
| Service | packaged arm64 nightly deb `rusty-photon-star-adventurer-gti_0.1.0+nightly.202609262026.g6eaa9b7_arm64.deb` from `nightly-packages` run [36269494520](https://github.com/rusty-photon/rusty-photon/actions/runs/36269494520) (sha256 `f3045e98c24a2475175a485978d73e106ef09e2951e1ea5c49c5753b3a5a96ef`), installed over `0.1.0+nightly.202609200943.gb226658`. Not a source build. Only this package was upgraded; the rest of the fleet stayed on the 2026-09-20 nightly |
| Platform | Raspberry Pi 5 Model B Rev 1.1 (BCM2712, aarch64), Raspberry Pi OS (Debian 13 trixie), kernel `6.18.34+rpt-rpi-2712` — the telescope field rig |
| Mount | Sky-Watcher Star Adventurer GTi over USB — the motor board's STM32 virtual COM port (`/dev/serial/by-id/usb-STMicroelectronics_STM32_Virtual_ComPort_4E8741795300-if00`, 115200 baud, `polling_interval` 200 ms). ASCOM `UniqueID` `b7afdd2e-851a-4b82-9e9a-2258ff652821` — the identity the driver minted into `mount.unique_id` on this install (see the design doc §Device identity), as reported by `/management/v1/configureddevices` |
| Mount config | `flip_policy.enabled = true` (the point of the run), `auto_flip_during_tracking = false`, `cw_exclusion_zone` at the shipped default (0.95, 11.05 h), `site_latitude_deg = 32.7157` (the rig's real latitude, above ConformU's `SIDE_OF_PIER_INVALID_LATITUDE` gate of 10°), `min_altitude_degrees = 0`, `unpark_from_ap_position = ap_park_3`, `settle_after_slew = 2s`. The retired `flip_range_hours` key was deleted before the upgrade — the new build refuses a config that still carries it — and the new build's `doctor` reported the file clean (1 ok, 0 warn, 0 fail) |
| Transport to the device | `conformance` over the service's **production HTTPS + HTTP Basic endpoint** (Let's Encrypt wildcard verified against the system store; `AccessServiceType: Https` plus the observatory credential in the settings file). `alpacaprotocol` through the loopback header-injecting proxy of the [2026-08-08 record](../2026-08-08-svbony-camera-sv605cc-rig/README.md), because ConformU 4.5.0's embedded client-library calls carry no credentials; the raw protocol requests authenticate end to end |
| ConformU | 4.5.0 build 53834 (`49ab847`, the `v4.5.0` tag), `linux-arm64`, run **on the rig itself** as a transient systemd unit, invoked as `conformu conformance <url> -s <settings>` — the form [hardware-validation.md](../../skills/hardware-validation.md) prescribes. **Every test group ran.** The settings file carried the in-tree `tests/conformu_integration.rs` selection (`TestSideOfPierRead = true`, `TestSideOfPierWrite = false`, `TelescopeExtendedRateOffsetTests = false`, `TelescopeFirstUseTests = false`), but the URL-argument `conformance` verb calls ConformU's `SetFullTest()` before running — its own help text reads *"with all tests enabled"* — so those four flags were overridden, and the `SideOfPier Write` flip test, the side-of-pier model tests, the first-use and the extended rate-offset groups all executed. Only the `conformance-settings` / `alpacaprotocol-settings` verbs honour test selection (`crates/bdd-infra/src/conformu.rs` documents the same behaviour). The file's connection settings (`Https`, credentials) and tolerances (1″ in Dec, 0.07 s in RA) were used as written. The in-tree integration test passes the same flags to the same verb, so they are inert there too — [#1337](https://github.com/rusty-photon/rusty-photon/issues/1337) |
| Operations | rp stopped for the duration (one orchestrator at a time), operator at the pier with a hand on the power throughout. Start state: mount idle, tracking off, at the Park 3 pose (encoders RA −907200 / Dec +725760 ticks). Because the full test set runs, a mount with `CanSetPierSide = true` **will** be put through the `SideOfPier Write` test: slews to HA −3 at Dec 0, then to two minutes east of the meridian, tracks across it for seven minutes, then flips through the pole — plan the sweep for it |

Log timestamps are the rig's local time (PDT, UTC−7): the run spans
17:32–17:53 on 2026-09-26, i.e. 00:32–00:53 UTC on 2026-09-27.

## Verdicts

- **`alpacaprotocol`** — *"Found 0 errors, 0 issues and 3 information
  messages"*: [alpacaprotocol.log](alpacaprotocol.log). The three
  information items are the `PUT Park` ID-casing variants answered
  `200` with `InvalidOperationException: park refused: slew already in
  progress` — ConformU issues Park three times back to back and the driver
  refuses a second Park while the first one's slew is still running. The
  2026-09-20 run on the rig recorded the identical three.
- **`conformance`** — **0 errors, 11 issues, 0 configuration alerts, 0 timing
  issues**, and every one of the 93 timed members inside its target
  (*"Congratulations, all members returned within their target response
  times"*): [conformance.log](conformance.log), machine-readable
  [conformance-results.json](conformance-results.json). The issue list is
  analysed below.

## The thing under test: Dec pulse direction on both pointing states

5 s pulses at the default guide rate (0.5 × sidereal), expected ±37.6″, tolerance 1″:

| Leg | Mechanical HA | Dec encoder | Pointing state | North | South |
|---|---|---|---|---|---|
| HA −9 | −8.99 h | +85.0° | counterweight-down | **+37.9″** | **−38.4″** |
| HA +9 | −2.99 h | +95.0° (through the pole) | **counterweight-up** | **+37.9″** | **−37.9″** |
| HA −3 | −2.99 h | +45.0° | counterweight-down | **+37.5″** | **−37.9″** |
| HA +3 | −8.99 h | +135.0° (through the pole) | **counterweight-up** | **+37.9″** | **−37.9″** |

All eight pulses moved the right way, within 0.8″ of the expected distance.
The pier side per leg is not something ConformU prints; it was taken from the
driver's own wire trace — the `:j1` / `:j2` encoder replies in the journal at
the instant of each pulse, decoded against CPR 3,628,800 (RA) and 2,903,040
(Dec). Both HA +3 and HA +9 sat with |Dec encoder| > 90°, i.e. the OTA swung
past the pole: that is the pointing state #1300 reported inverted (North
−37.5″ / South +37.5″ against the mock) and the one the design doc had, until
this run, called derived rather than measured. The mount reached them exactly
as predicted — the counterweight-down solution for HA +3 / +9 lies inside the
CW exclusion zone, so the flip-aware planner took the through-the-pole side
(mechanical HA −9 / −3).

Nothing before this run had measured a counterweight-up Dec pulse on
hardware. The previous rig run left on the box (2026-09-20, a build predating
PR #1298 — it still shows the #1295 1.25× overshoot at +46.9″ / −47.3″) hit
the CheckMethods abort at HA +9 (*"target mech_HA 9.000 h is inside the CW
exclusion zone"*) and never got to the flipped side; #1301 removed that abort.

## The 11 issues, all RA

- **Seven × RA East/West magnitude — [#1299](https://github.com/rusty-photon/rusty-photon/issues/1299).**
  East +2.81 / +2.78 / +2.65 s at HA −9 / +9 / +3; West −1.91 / −2.36 /
  −2.36 / −2.26 s at HA −9 / +9 / −3 / +3; against ±2.51 s. East at HA −3
  passed (+2.52 s). Measured minus expected runs +0.01 … +0.61 s, mean about
  +0.25 s — the constant stop-window offset #1299 describes (≈ +0.24 s against
  the mock), with the hardware scatter on top. Unchanged by #1300 / #1301,
  which do not go near it.
- **Four × cross-axis RA change during a Dec pulse — [#1334](https://github.com/rusty-photon/rusty-photon/issues/1334)**,
  filed from this run. HA −9 South +0.07 s, HA +9 South −0.12 s, HA −3 North
  −0.17 s, HA +3 South +0.08 s (tolerance 0.07 s); the other four Dec pulses
  read ≤ 0.07 s and passed. The mock's prediction for this config has no such
  entries. All four are inside ±0.2 s, which is one 200 ms `polling_interval`
  of tracked sky: `RightAscension` combines the background poll's last
  encoder sample with a live LST, so a read is off by up to one poll of
  sidereal motion and two reads a few seconds apart can differ by that much
  with the RA axis doing exactly what it should. The 2026-09-20 run shows
  the same class (+0.09 s / −0.20 s at HA −9). Not #1299 (pulse magnitude)
  and not #1300 (Dec direction).

So the design doc's mock-derived "20 → 8" for this config measures **11** on
hardware: seven of the eight predicted RA offsets, plus four cross-axis
readings the mock cannot show.

## Also on record from this run

- **Meridian flip.** ConformU's `SideOfPier Write` test parked the mount on the
  meridian at Dec 0, tracked it through (mechanical HA −0.03 → +0.09 h over
  seven minutes), then asked for the other side: *"Successfully flipped
  pierWest to pierEast"*. The through-wrap flip and its state reporting are
  now on record with ConformU artefacts (the 2026-05-16 validation in the
  design doc predates the record trail).
- **Pointing state.** `SideofPier` and `DestinationSideofPier` model tests:
  `WE` at HA −9 / +9 and at HA −3 / +3 — `pierWest` east of the meridian,
  `pierEast` west of it, on both sides, and *"DestinationSideOfPier is
  different on either side of the meridian"*.
- **Park.** `Park` (16 s to the Park 3 pair), `Unpark`, the parked refusals,
  and `SetPark` all OK. `SetPark` persisted `park_ra_ticks = -907200` /
  `park_dec_ticks = 725760` into the rig's config — the Park 3 pair, identical
  to the `preferred_ap_park` fallback, so the rig's park pose is unchanged.
- **Slews and syncs** to the HA ±3 / ±9 legs (Dec 45° and 85°, through the
  pole where the planner chose that side), `SlewToCoordinates`,
  `SyncToCoordinates` and their bad-coordinate rejections, `AbortSlew` (stopped
  the mount in 0.6 s): all OK.
- **Timing.** 93 timed members, all inside target, with ConformU on the rig's
  own loopback TLS rather than across the WiFi link.

## Files

- [`alpacaprotocol.log`](alpacaprotocol.log) — unmodified. The device under
  test appears as the loopback proxy (`127.0.0.1:18117`).
- [`conformance.log`](conformance.log) — unmodified **except lines 5 and 7**,
  where the rig's hostname is replaced by the placeholder `<rig-host>`. The
  rig's address must not appear in this public repository; ConformU stamps the
  device URL into the log, so it was redacted before committing.
- [`conformance-results.json`](conformance-results.json) — unmodified (carries
  no host).

## State the rig was left in

Mount parked on the Park 3 tick pair (`Park` through the driver, 16 s, then
`AtPark = true`), tracking off, the ConformU client disconnected; rp restarted
and holding the mount connection again. The GTi config now carries the two
park keys ConformU's `SetPark` wrote and no `flip_range_hours`; the pre-run
file is beside it as `star-adventurer-gti.json.bak-20260926-1332`.

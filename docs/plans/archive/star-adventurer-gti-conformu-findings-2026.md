# star-adventurer-gti: ConformU findings, 2026

A record, moved whole from `docs/services/star-adventurer-gti.md` on
2026-11-12: what ConformU reported against the driver under configs
that do not flip, the issue counts per config and how the in-tree
test's count came down to zero, the `workflow_dispatch` runs that
preceded the package joining the nightly rotation with the one leg
that missed afterwards, and the report a disabled CW exclusion zone
produced. The design document states what the suite reports today.

## The findings and the counts per config

This passage stood under
[star-adventurer-gti.md § Running ConformU manually](../../services/star-adventurer-gti.md#running-conformu-manually),
after the statement that the in-tree test's mock config runs clean.

Other configs show three findings, one of them now fixed:

1. **The HA +9 pulse-guide leg aborted CheckMethods while flips
   were off.** With `TelescopeExtendedPulseGuideTests` forced on,
   ConformU's `CheckMethods` runs `TestPulseGuide` at mechanical
   HA −9, +9, −3 and +3 — before it reaches the side-of-pier model
   tests. Until 2026-10 meridian flips were opt-in and off by
   default, and without a flip the only solution for HA +9 sits
   inside the counterweight exclusion zone `(0.95, 11.05)` h, so the
   driver refused the slew (`target mech_HA 9.000 h is inside the CW
   exclusion zone`); the `InvalidValueException` is caught by
   ConformU's "Exception when testing device" handler at
   CheckMethods scope and the rest of the suite is abandoned, so the
   run ended 0 errors / 1 issue: the abandon (issue #1296). Flips are
   always on now and reach HA +9 through the pole, so CheckMethods
   completes; disabling the zone (`"cw_exclusion_zone": null`) also
   lets it complete, and exposes the other two findings below.
2. **`SideOfPier` returns `pierWest` for every in-envelope
   target.** `coordinates::side_of_pier` keys on
   `|dec_ticks| > cpr_dec/4` (the Dec-encoder-past-pole
   "physical flip" convention). Inside the safety envelope the
   Dec encoder never crosses that threshold, so the driver
   reports `West` regardless of which side of the meridian the
   mount is observing. ConformU asserts the ASCOM pointing-state
   convention (`pierWest` for HA ∈ [-6, 0), `pierEast` for
   HA ∈ (0, +6]) and records ISSUEs for every HA > 0 case in
   both `SideofPier` and `DestinationSideofPier`.
3. **PulseGuide misses ConformU's tolerance in RA.** ConformU
   expects `guide_rate × duration` of motion on the pulsed axis
   (5 s at the default 0.5 × sidereal: 37.6″ in Dec, 2.51 s in
   RA) and ~0 on the other, at HA ±3 and ±9. Dec North/South is
   within tolerance (37.5″) and in the right direction on both
   sides. RA East/West was issue #1299:
   - **RA East/West carried a constant offset of ≈ +0.2 s of RA**
     (mock: East +2.74 s, West −2.26 s against ±2.51 s; tolerance
     0.07 s). The rate itself was right — the scale solves to
     0.996. The offset was the pulse's own `:K1` + stop-and-wait,
     whose fixed 100 ms sleep ran once before the shifted rate and
     once before the sidereal restore while the sky drifted past.
     Hardware showed the same offset: pooled over every
     post-#1298 rig pulse it is +0.185 ± 0.024 s, two ~100 ms
     windows. An earlier reading of "larger on hardware, because a
     real motor decelerates" came from one noisy pair; `:K1` from a
     tracking rate stops the GTi in about 4 ms. Fixed: a pulse on a
     tracking RA axis now changes the rate of the running motor and
     never stops it, and a trim cancels the forward steps the motor
     board adds at each rate change (see
     [§PulseGuide lifecycle](../../services/star-adventurer-gti.md#pulseguide-lifecycle)).
   The Dec-direction defect this list used to carry — North
   moving south and South moving north at HA +3 and +9 with
   flips on, the right distance the wrong way — was issue #1300, fixed: Dec pulses now resolve `ccw`
   against the side the mount is on (see the Dec sign
   convention under
   [§PulseGuide lifecycle](../../services/star-adventurer-gti.md#pulseguide-lifecycle), including
   what a mock run does and does not establish). The
   counterweight-up direction is measured, not derived: the
   2026-09-26 field-rig run guided Dec the right way at HA +3
   and +9 with the mount through the pole — see
   [§Real-hardware validation](../../services/star-adventurer-gti.md#real-hardware-validation).

   Earlier revisions of this section recorded a much worse
   picture (Dec at ~2× the rate, RA West moving east). That was
   the mock, not the driver: its tracking-mode motion advanced a
   fixed number of ticks per `:j` poll and ignored the step
   period, so no pulse rate could be measured against it. The
   mock now honours `:I` (see [§Testing Strategy](../../services/star-adventurer-gti.md#testing)).
   The one real rate defect it had been hiding — a Dec step
   period derived from the RA axis' CPR, a 1.25× overshoot
   ConformU measured on hardware — is fixed.

Measured against the mock with ConformU 4.5.0, per mount config
(0 errors in every run):

| Config | Issues | What they are |
|---|---|---|
| flips off (the retired `flip_policy.enabled = false`), trim zeroed | 3 → 1 | ~~RA East/West offset at HA −9 (2)~~ — fixed; failure (1) abandons CheckMethods (1) |
| `cw_exclusion_zone: null` | 13 | RA East/West offset at HA ±3, ±9 (8), measured before the fix; failure (2) `SideofPier` / `DestinationSideofPier` (5) |
| flips on (the in-tree test's config; the only behaviour since 2026-10) | 20 → 7–8 → **0** (see below) | ~~RA East/West offset (7–8)~~ — fixed; ~~Dec direction on the flipped side (4)~~ — fixed; ~~slews / syncs to HA +1…+4 h rejected as inside the CW exclusion zone (8)~~ — fixed |

Flipping clears failures (1) and (2) outright —
`SideOfPier Write` flips, and `SideofPier` /
`DestinationSideofPier` report the pointing state at HA ±3 and ±9.

The eight rejections that row recorded came from the pier-side
selector: from the counterweight-up side, a target outside the
`flip_range_hours` window was sent to the counterweight-down side,
where HA +1…+4 h lies inside the exclusion zone, although the
counterweight-up side could reach it. That is issue #1301, fixed —
the selector now picks the side by reachability
([§Pier-side decision tree](../../services/star-adventurer-gti.md#pier-side-decision-tree)) and sync
resolves against the side the mount is on
([§Sync and pier side](../../services/star-adventurer-gti.md#sync-and-pier-side)). The four Dec-direction
issues are #1300, also fixed — Dec pulses resolve `ccw` against the
side the mount is on, and that fix is measured on hardware on the
counterweight-up side, not only against the mock (see
[§Real-hardware validation](../../services/star-adventurer-gti.md#real-hardware-validation)). **The `20`
above was measured against the mock before those fixes; the `7–8` is
a mock re-run on 2026-09-27** (ConformU 4.5.0): every remaining issue
is a #1299 RA East/West offset at HA ±3 / ±9. The full URL-verb run
counted 7 and a settings-verb run of the same eight legs counted 8:
the HA +9 East leg read +2.53 s in the first (0.01 s from the
expected +2.51 s, a pass) and +2.74 s in the second, while the other
seven legs sat at +2.71…+2.76 s East / −2.24…−2.26 s West in both —
scatter on one leg, not a value near the 0.07 s tolerance. With #1299
fixed the same run reads **0 issues** (2026-09-29): every East/West leg
at +2.49…+2.52 s / −2.51 s against ±2.51 s (differences 0.00–0.02 s),
North/South at ±37.5″. The mock adds no edge steps and the test's
config zeroes the trim to match, so the clean run shows the pulses no
longer lose angle to the old fixed stop waits. It cannot show that RA
is never stopped: the mock stops an axis instantly, so a stop costs no
angle here. The frame tests pin that
(`a_tracking_ra_pulse_sends_only_its_two_rate_frames` and the BDD frame
counts). What the GTi's board does at a rate change is the rig's to
measure. On hardware the same config
measured **11** on 2026-09-26: seven of the eight RA offsets (East at
HA −3 passed, at +2.52 s) plus four cross-axis RA readings of
0.07–0.17 s during Dec pulses that the mock never shows — issue
#1334, a poll-snapshot-versus-live-LST read artefact, neither #1299
nor #1300. The driver now carries each sample forward to the read
instant ([§Encoder samples and the read instant](../../services/star-adventurer-gti.md#encoder-samples-and-the-read-instant)),
so those four should read ≈ 0.00 s; that is measured against the mock,
and a rig re-run is still owed. The mock never showed them because it
stops an axis instantly and a 5 s pulse is exactly 25 polls at 200 ms:
the reads before and after a pulse fell on the same poll phase and the
artefact cancelled. The integration test's mock config therefore polls
at 300 ms, which does not divide 5 s.

## The dispatch runs and the leg that missed

This passage stood under
[star-adventurer-gti.md § In the nightly rotation](../../services/star-adventurer-gti.md#in-the-nightly-rotation).

Two `workflow_dispatch` runs of the workflow on 2026-10-07, before the
package joined the rotation, all 0 errors / 0 issues / 0 configuration
alerts:

| Runner | Suite | Parallel step (Linux / macOS) or job step (Windows) | Worst RA East/West leg |
|---|---|---|---|
| Linux, run 1 | 710 s | 13.5 min | 0.03 s |
| Linux, run 2 | 707 s | 14.5 min | 0.04 s |
| macOS, run 1 | 713 s | 15.8 min | 0.04 s |
| macOS, run 2 | 711 s | 14.2 min | 0.02 s |
| Windows, run 1 | 708 s | 11.8 min | 0.02 s |
| Windows, run 2 | 708 s | 11.8 min | 0.03 s |

The package is the rotation's long pole: the same day's scheduled run,
without it, finished the parallel step in 4.4 min on Linux and 6.2 min
on macOS, against the step's 30-minute budget. On the shared runners
the RA legs came about five minutes into the step, while two to four
other suites were still running (zwo-camera's image downloads among
them on Linux), and stayed inside the scatter the idle host and the
Windows jobs showed: 48 legs, none worse than 0.04 s against 0.07 s.

The first leg to miss came on 2026-10-10, on Windows, where the suite
has the runner to itself
([#1457](https://github.com/rusty-photon/rusty-photon/issues/1457)).
HA −3 West read −2.77 s against the expected −2.51 s. Two measurements
put the cause at a late restore:

- At the default 0.5× guide rate, the extra 0.26 s of RA is the pulse
  running about 0.52 s long.
- ConformU waited about 0.55 s longer for `IsPulseGuiding` to clear
  than on any other Windows leg.

The other 144 East/West legs since the package joined the rotation
(six runs, three OSes) sat within 0.04 s.

## The report of a config that does not flip

This passage stood under
[star-adventurer-gti.md § Expected ConformU report](../../services/star-adventurer-gti.md#expected-conformu-report).

With the in-tree test's config the report is clean: 0 errors,
0 issues, 0 configuration alerts. The findings below belong to configs
that do not flip: (1) to the flips-off default the driver shipped
until 2026-10, (2) to that default and to a disabled CW exclusion
zone, under which the selector never changes sides; (3), issue #1299,
is fixed. The
per-config issue counts are in the table under
[§Running ConformU manually](../../services/star-adventurer-gti.md#running-conformu-manually).

After (1) is worked around by disabling the CW exclusion zone
for the mock test config (`"cw_exclusion_zone": null`), the report
before the #1299 fix read:

- **0 errors** — anything here is a real driver regression.
- **13 issues**:
  - PulseGuide East / West "Moved {east,west} but outside test
    tolerance" at HA ±3 and ±9 (8 entries) — driver bug (3),
    the RA stop-window offset, now fixed. North / South pass, and
    no pulse moves the axis it is not supposed to.
  - `SideofPier` and `DestinationSideofPier`
    "`pierWest` is returned when the mount is observing at an
    hour angle between 0.0 and +6.0" (2 entries) — driver
    bug (2), on a mount that does not flip. ConformU's `SideofPier` test assumes a
    flip-at-meridian GEM (EQMOD / Sky-Watcher Synscan /
    ASCOM-driver-pattern behaviour) and expects `pierEast`
    for any target west of the meridian; the driver returns
    `pierWest` everywhere because the safety envelope keeps
    the Dec encoder within `[-CPR/4, +CPR/4]` and
    `coordinates::side_of_pier` only flips when that bound is
    crossed.
  - `SideofPier` / `DestinationSideofPier`
    "reports physical pier side rather than pointing state"
    and "Same value … on both sides of the meridian"
    (3 entries) — same root cause as bug (2): a non-flipping
    mount lands every in-envelope target in the same
    pointing state.

Historical baselines (`alpacaprotocol`-only or partial
`conformance` runs):

- Pre-#202 baseline (HA-meridian split, `DestinationSideOfPier`
  unimplemented): 9 issues — `DestinationSideOfPier` ×1
  (NotImplemented), `SOPPierTest` ×4 (inherited from
  NotImplemented), `SOPPierTest` ×2 (safety envelope),
  `TrackingRate Write` ×2 (upstream `ascom-alpaca-rs`
  framework bug — Alpaca enum rejection at the axum/serde
  layer before reaching the driver).
- Post-#202 baseline (Dec-encoder split, `DestinationSideOfPier`
  implemented, PulseGuide not yet landed, conformance never
  reached SideOfPierTests because alpacaprotocol failed on
  `IsPulseGuiding`): 7 issues as previously documented.

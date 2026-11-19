# Star Adventurer GTi: post-pickup RA accuracy experiments

## Status

**Status: COMPLETE (archived 2026-05-24).** B + D landed on PR #210 (2026-05-12).

Real-hardware ConformU residuals before / after:

| Metric | Before (PR #210 `:K` only) | After B + D |
|---|---|---|
| Mean RA residual | 8.4″ | **3.7″** |
| Max RA residual | 11.0″ | **6.6″** |
| 10″ tolerance crossings | 2 / 10 slews | **0 / 10** |
| Pickup iterations / slew (typical) | 5 (saturated) | **1–2** |

Experiments A and C remain on the shelf (faster polling, more
iterations) for if further accuracy improvements are ever wanted —
but B + D alone get us cleanly under tolerance.

## Motivation

The first real-hardware ConformU run after PR #210 landed `:K` showed
SyncToCoordinates and SyncToTarget crossing ConformU's ±10″ RA tolerance
on a couple of tests (10.2″ and 10.95″). Investigation against the wire
log (see PR #210 conversation) ruled out slew distance and the `:K` vs
`:L` choice as the cause — the residuals cluster ~5–11″ across slews of
wildly different sizes, all with a consistent **positive** sign
(`Actual > Target`). The signature is LST drift accumulated *after* the
pickup loop converges, dominated by:

- Pickup-loop exit tolerance (5″) → up to 5″ residual at pickup exit
- Tracking-restart wire latency (~50 ms) → ~0.7″
- `settle_after_slew` (200 ms) — only adds drift if firmware tracking
  hasn't actually engaged yet → up to ~3″
- ConformU's HTTP poll latency between `Slewing == false` and
  `RightAscension` read → ~1.5″

Total floor: ~5″ + 5″ ≈ 10″ worst case, which is exactly the
tolerance line. The 2 ISSUE crossings out of 10 RA reads are bare
variance.

This plan walks three independent fixes (and a combined run) to drive
the floor down well under the tolerance line. None of these are part
of issue #207; they are follow-up work.

## Pass criterion

For each experiment, run conformu against the real GTi and compare RA
residuals on the named tests:

- **SlewToCoordinates / SlewToCoordinatesAsync** (first slews from park)
- **SlewToTarget / SlewToTargetAsync** (target-based variants)
- **SyncToCoordinates / SyncToTarget** "Slewed to start position" and
  the 2× "Slewed back to start" sub-steps

**Pass:** all RA residuals < 7″, zero crossings of 10″ on a single run.

## Setup: characterise baseline variance

Before any code change, run conformu against real hardware **2× more**
(3 total including the morning-of-2026-05-12 run) without modification.
Goal: know how much the same test bounces between runs. If
`SyncToCoord slew-to-start` is 10.2″ in run 1, then 7.8″ in run 2, then
9.5″ in run 3, the noise floor is ~2.4″ peak-to-peak and any fix needs
to deliver more than that to be measurable.

Capture per run:

- `/tmp/conformu-real-baseline-N.log` (conformu stdout)
- `/tmp/service-debug-baseline-N.log` (service wire trace at `-l debug`)

Tabulate per-test residual, signed (Actual − Target in arcsec).

## Free diagnostics — DONE

Both diagnostics were run on the 2026-05-12 real-hardware log. Results
materially change the experiment plan below.

### D1. Pickup iteration count — RESULT: iteration-capped on ≥10/11 slews

Across the 11 slews captured in the run, the pickup loop **exhausts
`PICKUP_MAX_ITERATIONS = 5` on essentially every slew** and exits with
RA residual stuck at 5–7″. Sample (slew 1):

```
iter 1  t+   0ms  ra=31.74"
iter 2  t+ 399ms  ra= 6.06"
iter 3  t+ 799ms  ra= 6.09"
iter 4  t+1200ms  ra= 5.80"
iter 5  t+1600ms  ra= 5.49"  (exhausted)
```

Every iteration takes ~400 ms (200 ms watcher poll + small re-slew +
wire round-trips). During 400 ms, LST advances ~6″. The pickup
corrects ~25–30″ per iteration but loses ~6″ to LST drift each round.
**The residual floor IS the per-iteration LST drift**, which is why
tightening `PICKUP_TOLERANCE_ARCSEC` cannot help — pickup physically
can't converge under the drift it accumulates between iterations.

This invalidates Experiment 1 below. The real fixes are either
(a) faster iterations or (b) pre-compensating each iteration's target
for the expected next-iteration LST drift.

### D2. Firmware tracking-engagement latency — RESULT: ~160 ms

Watcher's post-slew `:G110 :I108CC05 :J1` at 03:02:06.495 (UTC):

| Event | Encoder | Δ ticks (200 ms) | Inferred rate |
|---|---|---|---|
| `:J1` ack | -453439 | – | – |
| +160 ms (status flips `running=true`) | -453436 | +3 | first motion |
| +360 ms | -453426 | +10 | 50/s |
| +560 ms | -453417 | +9 | 45/s |
| +760 ms | -453408 | +9 | 45/s (sidereal target: 42/s) |

The firmware engages tracking within ~160 ms of `:J1` and reaches
sidereal-rate motion by ~400 ms.

**Initial reading (pre-Option-1):** `settle_after_slew = 200 ms`
appeared just barely enough; trimming to 100 ms would risk reading
RA mid-engagement (~1–2″ of drift), 0 ms would risk ~2.4″.

**Updated reading (after Option 1 with early pause-guard
release):** the watcher releases the background-polling pause guard
right after the tracking-restart `:J1` and *before* the settle
delay. That lets the background polling task refresh the snapshot
during settle with already-tracking encoder data, so settle is no
longer load-bearing for snapshot freshness — it's just a
mechanical-stability margin. Empirically, 100 ms now produces 0/10
tolerance crossings on both USB (max 6.6″) and UDP (max 8.7″).

## Experiments (revised after diagnostics)

The original three experiments (tighten pickup tolerance, trim settle,
verify tracking engaged) are **mostly redirected** by D1 + D2:

- **Experiment 1 (tighten tolerance)** is dropped. D1 shows pickup
  cannot converge below the per-iteration LST drift (~6″), so
  tightening the exit threshold below 6″ achieves nothing — pickup
  exhausts iterations and exits wherever it is.
- **Experiment 2 (trim settle)** is downgraded to "informational
  only". D2 shows settle is approximately right-sized for the
  firmware's 160 ms engagement latency; trimming saves at most ~1″
  with real risk if engagement varies between runs.
- **Experiment 3 (verify tracking engaged)** is dropped. D2 shows
  engagement is consistent at ~160 ms, well-modeled by the fixed
  200 ms settle.

In their place: two new experiments targeting the actual bottleneck
identified by D1.

### Experiment A: tighten the watcher polling interval

**Files:**

- `services/star-adventurer-gti/conformu-test-config.json` (set
  `polling_interval` to `"50ms"` from the default `"200ms"`)
- No code change needed — the watcher and pickup loop already use
  `transport.polling_interval_for_watcher()`.

**Hypothesis.** Pickup iterations are gated by `polling_interval`
(currently 200 ms). Each iteration's LST drift = iteration_duration ×
15.04″/sec ≈ 6″. Tightening polling to 50 ms drops the iteration
duration to ~150–200 ms (poll sleep + wire round-trip + slew time),
which shrinks per-iteration drift to ~2.3–3″ and lets pickup converge
under 5″ before iteration cap.

**Procedure.**

1. `polling_interval = "100ms"`. Run conformu. Capture residuals +
   wire trace.
2. `polling_interval = "50ms"`. Run conformu.
3. Confirm pickup converges (last iteration's residual < 5″)
   on each slew. If yes, RA residuals on Sync tests should drop
   well under 10″.

**Watch out for.** Polling interval also drives the polling task's
`:f` / `:j` cadence. Faster polling = more wire traffic. At 50 ms,
we're issuing ~80 wire round-trips/sec just for status — could
saturate the USB serial port, increase wire latency, or starve
real commands. If wire latency goes up (`:K` → `:f` poll → ack
time inflates), the per-iteration savings shrink.

**Estimated cost.** ~20 min real-hardware (2 runs).

### Experiment B: pre-compensate pickup target for next-iteration LST

**File:** `services/star-adventurer-gti/src/mount_device.rs`, inside
`spawn_slew_completion_watcher`'s pickup block.

**Hypothesis.** Currently each pickup iteration computes its target
for `LST(now)`. The slew it issues takes ~400 ms to settle; by then
LST has advanced ~6″, and the encoder lands 6″ short of where it
needs to be when the *next* check happens. Pre-compensating the
target for the *expected next-iteration LST* (now + 1 poll cycle +
expected wire latency) makes each iteration aim for where the target
will be when we re-check.

**Implementation sketch.**

```rust
// Inside the pickup block, when computing new_mech_ha:
let lst_now = local_sidereal_time_hours(SystemTime::now(), ...);
// Project forward by one iteration duration. The "expected next
// iteration time" = polling_interval + small wire round-trip budget.
let lst_target = lst_now + (polling_interval.as_secs_f64() / 3600.0);
let new_mech_ha = ra_to_mechanical_ha(target_ra, lst_target);
```

This is a one-line change (just the `lst_target` calculation).

**Why this is independent of polling interval.** Even at 200 ms
polling, pre-compensation lets pickup land *ahead* of the moving
target; by the time the next iteration measures, encoder + LST drift
land back on target. Pickup converges to noise instead of drift floor.

**Procedure.**

1. Implement the one-line pre-compensation change.
2. Add unit test (mock + faked LST) confirming pre-compensation
   makes the iteration target move forward by the polling interval
   in arcseconds.
3. Run conformu against real hardware once. Capture residuals.

**Watch out for.** If pre-compensation is too aggressive (over-shoots
the LST projection), pickup oscillates instead of converging.
Concretely: aim slightly behind one iter, then slightly ahead next.
If we see iteration residuals alternating signs, the projection
constant needs tuning.

**Estimated cost.** Code change + unit test + ~10 min real-hardware
(1 run).

### Experiment C (optional): increase `PICKUP_MAX_ITERATIONS`

**File:** `services/star-adventurer-gti/src/mount_device.rs`,
`PICKUP_MAX_ITERATIONS` constant (currently 5).

**Hypothesis.** Only useful if combined with A or B — alone, more
iterations of the same drift-chasing race don't help. But if A or B
makes individual iterations converge, more iterations is "free
insurance" that the cap doesn't bite. INDI uses 5; we may want 10
on real hardware because our iterations are slower than INDI's
(INDI runs the watcher in C without async-task overhead).

Skip unless A and B together can't get all residuals under 7″.

### Experiment D (informational): trim `settle_after_slew` to 100 ms

This was Experiment 2 in the original plan; D2 nearly invalidated it.
Worth running once *after* A and B to see if combining a slightly
smaller settle gains anything when pickup is no longer the bottleneck.

`settle_after_slew = "100ms"` (was `"200ms"`). Run conformu once.
Expected gain: 0–1″. If residuals get worse, revert.

**Estimated cost.** ~10 min real-hardware (1 run).

## Combined experiment

Take the best parameters / approach from 1, 2, 3 — apply all
simultaneously. Run conformu once.

**Pass criterion.** All RA residuals < 7″. Zero crossings of 10″.

If we still cross 10″ on one or two tests, that's the floor we can
hit without restructuring the watcher / pickup loop architecture.
Document it.

## Estimated total cost (post-diagnostic revision)

| Phase | Runs | Wall-clock |
|---|---|---|
| Diagnostics D1, D2 | 0 | DONE — free |
| Baseline characterisation | 1 | ~10 min |
| Experiment A (tighten polling) | 2 | ~20 min |
| Experiment B (pre-compensate target) | 1 | ~10 min + code |
| Experiment C (more iterations) | 0–1 | ~10 min (only if needed) |
| Experiment D (trim settle) | 1 | ~10 min |
| Combined | 1 | ~10 min |
| **Total** | **6–7** | **~60–70 min real-hardware + code + analysis** |

The reduction comes from D1's finding: tightening pickup tolerance
(originally 2 runs) was unwinnable; trimming settle (originally 3
runs) was nearly unhelpful. Both are dropped or downgraded.

## Output

For each run, capture:

- Per-test signed RA residual (arcsec)
- Per-test Dec residual (arcsec)
- Pickup iteration count (from wire log)
- Tracking-engagement latency (from wire log)
- Conformu issue count, errors count, configuration-alerts count

Tabulate in a follow-up doc / PR description. The "good" result
becomes the new conformu-test-config.json default for the driver.

## References

- PR #210 conversation for the diagnostic walk-through that led to
  this plan
- `docs/services/star-adventurer-gti.md` §"Post-slew tracking-pickup"
  (Phase 4 finding) and
  [§"Phase 4 driver-logic changes"](star-adventurer-gti-phase-record.md#phase-4-driver-logic-changes-that-real-hardware-required)
  (current pickup loop)
- INDI eqmod `eqmodbase.cpp` `RAGOTORESOLUTION` / `DEGOTORESOLUTION`
  (= 5″) and `GOTO_ITERATIVE_LIMIT` (= 5)

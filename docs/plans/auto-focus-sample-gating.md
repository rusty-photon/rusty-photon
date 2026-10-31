# Auto-focus sample gating and confirmation — trust the sweep only where it can see stars

## Goal

`auto_focus` fits a parabola to every non-null HFR sample of the sweep,
weighted by the frame's star count, and moves the focuser to the vertex
without checking anything. On a coarse sweep that reaches far defocus
this converges confidently on the wrong position: the first coarse pass
on the Starfront rig (issue
[#1187](https://github.com/rusty-photon/rusty-photon/issues/1187), a
±1200-step sweep at 300-step increments) reported a best position 160
steps and four pixels of HFR away from focus, while the star counts of
the same sweep pointed straight at it.

The mechanism has three layers, and the fix has one layer per phase:

1. **Measurement.** A star's HFR is accumulated over the thresholded
   component's own pixels, so it is capped by the component's area
   (roughly `0.4·√area` for a uniform blob). At far defocus the donut
   annulus fragments at the detection threshold; what survives the area
   filter is arcs of the ring and hot-pixel clusters, and they read 4–5
   px whatever the star's true size (26–30 px on that sweep). The wing
   samples are systematically wrong, not noisy, and the collapse in
   `star_count` is the only visible tell. This is the true root. Its
   hot-pixel part was [#1179](https://github.com/rusty-photon/rusty-photon/issues/1179); the rest is
   [#1684](https://github.com/rusty-photon/rusty-photon/issues/1684).
2. **Fit.** Linear star-count weighting cannot neutralise two low points
   on the wrong side of the V (5–12 stars against 370 is not enough),
   and a parabola is the wrong shape over ±1200 steps anyway. Replaying
   the sweep through the current fit reproduces the reported vertex
   with a weighted R² of 0.06.
3. **No self-check.** Nothing reports the fit's quality, nothing
   measures the position the focuser ends at, and a failed run leaves
   the focuser at the far end of the grid. A wrong success also poisons
   the downstream baseline: the deep-sky workflow stores the fitted HFR
   as the reference for HFR-triggered refocus.

The outcome of this plan: `auto_focus` rejects samples the detector
could not see, reports how well the curve fits, verifies the position it
moved to with a fresh frame, ends every run at a position it has
measured, and reports all of that in the result so an orchestrator can
tell a clean V from a guess.

## Implementation Status

| Phase | Description | Status | Branch / PR |
|-------|-------------|--------|-------------|
| G1 | Sample gating on star count, weighted R² in the result, confirmation frame with fallback to the lowest accepted sample, starting-position restore on fit failure, `final_hfr` for consumers | Merged | [#1193](https://github.com/rusty-photon/rusty-photon/pull/1193) |
| G2 | Hyperbolic V-curve model (`a·√(1 + ((x − x₀)/b)²)`, the centre written `x₀` so it does not collide with the obstruction-derived blur constant `c` of the focus-model plan's D9); the capture sweep's lands in the `focus-model` provider ([plan](focus-model.md), D13 and S9), the guide-metric variant's in `rp`; R² gets a threshold knob in the provider, unset by default | Merged — focus-model plan S9 | [#1675](https://github.com/rusty-photon/rusty-photon/pull/1675) |
| G3a | Hot-pixel rejection in the detector: a 3×3-median suppression before smoothing and a 0.25 px HFR floor ([rp.md](../services/rp.md), `measure_basic` algorithm steps 3 and 8), validated on rig2's 2026-09-08 and 2026-09-26 frames and its binned 2026-09-09 ones. Edge rejection is dropped: in those frames, the hot-pixel detections in each 30 px edge strip are in proportion to its area, so there is no edge band to reject | Merged | [#1473](https://github.com/rusty-photon/rusty-photon/pull/1473), [#1478](https://github.com/rusty-photon/rusty-photon/pull/1478) |
| G3 | Measurement-side: aperture HFR around the centroid, two-star minimum per point, aggregation over the N brightest stars, and whether a confirmation frame below the sparse gate rejects the fit or is inconclusive — tracked under [#1684](https://github.com/rusty-photon/rusty-photon/issues/1684) | Not started; G2 has merged, so nothing blocks it | |

G1 first; G2 after G1 because an R² threshold against a parabola rejects
clean fine sweeps (a parabola scores 0.70 on the rig's clean fine sweep,
below the 0.7 NINA uses as its default gate). G3 has no technical
dependency on either, but is sequenced after G2 (see the decision
below), and must be validated on real frames, never on synthetic ones —
the sweep frames from the night this was found are still on the rig.

**Decided 2026-09-13.** G2 lands next, in the `focus-model` provider
([plan](focus-model.md) S9), and does **not** wait for another rig
night. The hyperbola is the analytic defocus curve — a geometric cone
with a seeing floor — so what makes it the right model is physics that
holds across optical trains, not one rig's numbers; fitting it to a
single night's sweeps is precisely what would make it train-specific.
It is validated against curves generated over a spread of focal
ratios, `microns_per_step` values and seeing floors, with the rig's
two recorded sweeps kept as regression cases.

The R² threshold is the opposite kind of claim. How clean a real sweep
looks is a property of a rig — its seeing, its sampling, its star
field — so a single default cannot be right everywhere, and a
too-tight one turns usable sweeps into failed runs on a live night.
It therefore ships as `min_fit_r_squared`, defaulting to unset: fit
quality is reported, never enforced, until a train's own numbers
justify a number for that train. The knob is the provider's alone, on
its per-train `trains.<id>` block: `rp`'s guide-metric sweep takes the hyperbola
(focus-model plan S9) and keeps reporting `fit_r_squared` with no
threshold and no `poor_fit`, because the guiding train's `auto_focus`
block is the one `rp` keeps after that plan's S7 and this plan adds no
knob to it. Null is the opt-out; a value that is
present is validated at load like the other per-train knobs — finite
and within `[0, 1]`, the range a coefficient of determination can
take. The endpoints are legal and mean what they say: `0` accepts
every fit, the same as unset but written down, and `1` accepts only an
exact fit, so the check catches a value outside the range, not a poor
choice inside it. That reading of `0` rests on the reported R² being
clamped to `[0, 1]`, which the parabola does today
(`services/focus-model/src/sweep.rs`, a raw coefficient of
determination can go negative on a fit worse than the mean); G2's
hyperbola keeps the clamp, as a requirement and not an accident of
the port. The #1187 failure is already caught
without it: the 0.1 gate leaves three accepted samples, below the
default `min_fit_points` of 5, so a run **at the defaults** ends in
`not_enough_stars` and G1's decision 5 restores the starting position.
Decision 1 above quotes a vertex 21 steps from focus for the same
sweep; that is an offline replay fitting those three samples, which
needs a `min_fit_points` below the shipped default of 5. Both are
true — the replay shows the gate picks the right samples, the default
refuses to fit so few of them — and neither describes a sweep that
reaches a confirmation frame. Note what that
is *not* — `retry_centre` holds the same centre for this sweep, whose
stars ran out on both sides (`services/focus-model/src/sweep.rs`
shifts after `not_enough_stars` only when they ran out on one side
and the accepted samples fall strictly away from it, focus-model plan
D13 step 5), and the confirmation frame is
never reached because no fit is produced. Refusal and restore are the
safety here, and an R² gate would add nothing: at the defaults no fit
exists for it to judge, and at `min_fit_points` 3 three samples meet
three parameters — the parabola exactly, R² 1, and G2's hyperbola
exactly when one passes through them. Three observations need not
admit a hyperbola, and what the solver does then was S9's to define.

*Settled in S9.* Where no hyperbola passes through three samples, the
fit is the nearest V: the limit of the family as the focused HFR goes
to 0, at an R² below 1. That is a fit, not a failure, and a
configured threshold judges it like any other
([focus-model.md § The sweep](../services/focus-model.md#the-sweep),
step 4). Refusing it would refuse exactly the sweep a coarse grid
makes, whose samples all sit far enough from focus that the floor
never shows. The #1187 sweep is that case: at `min_fit_points` 3 its
three accepted samples admit no hyperbola, and the nearest V centres
at 29767, a step from focus, at R² 0.998 — a fit a gate would pass,
and a better one than the parabola's 21 steps. At the defaults
nothing changes: three samples are refused before any fit.

G3 follows G2: sample gating and the confirmation frame currently
paper over a measurement bug, and the hyperbola's wings are fitted to
exactly the samples [#1684](https://github.com/rusty-photon/rusty-photon/issues/1684) would make honest. Its frame corpus is the
one hardware dependency left in this plan.

G3a, the hot-pixel part of G3, lands ahead of G2 at the owner's
direction (2026-10-11). It changes no result at the settings the rig
ran (`threshold_sigma 10`, `min_area 40`). It is what lets a sweep run
at a `min_area` that admits compact stars: at `threshold_sigma 8` and
`min_area 6`, the 2026-09-26 sweep's samples measure 36–202 stars and
trace the V-curve, where every sample measured a median HFR of
0.002–0.003 px before. Its regression fixture is a 512×512 crop of that
sweep's best-focus frame (`services/rp/tests/fixtures/star_detection/`).

Each phase follows
[development-workflow.md](../skills/development-workflow.md): design-doc
update (rp.md, session-runner.md) first, BDD second, code third.

## Decisions (fixed — settled 2026-09-08)

1. **Gate samples on the sweep's own star counts.** A sample whose
   `star_count` is below `min_star_fraction × max(star_count over the
   sweep)` is rejected as `sparse` before the fit. The fraction is
   relative, so a sparse field with five stars total is never gated;
   only a collapse relative to the sweep's best frame is. Default 0.1
   (that value keeps exactly the three dense samples of the #1187
   sweep and lands the vertex 21 steps from focus). Rejected samples
   stay in `curve_points` with a `rejected` reason and do not count
   toward `samples_used` or `min_fit_points`. Capture sweeps only: the
   guide-train sweep's per-position frame count is nearly constant and
   carries no such signal.
2. **Report fit quality, do not enforce it yet.** The result carries
   `fit_r_squared`, the weighted coefficient of determination over the
   accepted samples. A threshold waits for G2.
3. **Confirm the fitted position with a fresh measurement.** After the
   move to `best_position` the tool takes one more frame (one more
   guide sample set on the guiding train) and measures it. The
   confirmation is accepted when the frame has stars, its star count
   passes the same gate as the sweep, and its HFR is at most
   `(1 + confirmation_tolerance)` times the lowest accepted sweep
   sample. Default tolerance 0.25: a good fit measures within a few
   percent of the best sample plus seeing jitter; the #1187 failure
   measured twice the best sample. Always on — one exposure per run is
   the price of never trusting a fit blindly.
4. **Fall back, do not fail, when the confirmation is rejected.** The
   focuser moves to the lowest accepted sample's position and the run
   succeeds with `confirmed: false`. Two positions have been measured
   and the better one wins; the alternative (error, focuser back at an
   arbitrary starting point) is a worse night. The old rule "never move
   to the lowest sample, it is unverified" no longer holds once the
   fitted position has been measured against it.
5. **A failed fit restores the starting position.** `not_enough_stars`
   and `monotonic_curve` used to leave the focuser at the last grid
   position, the far end of the sweep. The tool now moves back to the
   position it started from, best effort: a failed restore is logged
   and the fit error is still the one reported.
6. **Consumers read the measured position, not the fitted one.**
   `final_position` and `final_hfr` (`final_hfd` on the guiding train)
   are where the focuser is and what was measured there; `best_position`
   and `best_hfr` keep their fitted meaning. `focus_complete` and the
   `refocus_complete` steps carry both plus `confirmed`; the deep-sky
   workflow stores `final_hfr` as its refocus baseline.
7. **No windowed refit in G1.** Replaying the rig's sweeps showed that
   restricting the parabola to the points around the minimum adds
   nothing once sparse samples are gated (21 vs 21 steps on the coarse
   sweep) and is a wash on the fine sweep (25–37 steps off either way,
   because that V is asymmetric). It would change `samples_used` on
   every clean sweep for no gain. The hyperbola in G2 is the right tool
   for the wings.

## Deferred

- A fallback to the lowest accepted sample when the gate leaves fewer
  than `min_fit_points` samples, proposed during G1's review, is
  superseded: the bounded retry of
  [#1202](https://github.com/rusty-photon/rusty-photon/issues/1202)
  (focus-model plan, S1) repeats the sweep and shifts it toward the
  minimum instead, and the fit-failure error now carries the curve.
- The in-call two-stage sweep (coarse locate, then a fine grid around
  the locator). The contract deliberately declines re-sweep state
  machines; with gating plus confirmation a coarse pass now ends within
  one step of focus or says it did not, and the orchestrator runs the
  fine pass.
- Outlier rejection on fit residuals (Hocus Focus's Grubbs test) and
  per-point error bars from the HFR spread across stars. Both need the
  measurement layer (G3) to produce honest spreads first.
- Aggregating over the N brightest stars instead of the median over
  every detection. Part of G3.

## What NINA and Hocus Focus do (for reference)

- NINA's sweep is adaptive: it starts near focus and extends each side
  until it has four trend-line points there. A frame with no stars stays
  in the list with a huge error bar; star count otherwise plays no role.
- NINA validates: default hyperbolic fit, R² threshold 0.7, and a fit
  below it or a vertex outside the sampled range fails the run and
  restores the initial position. On the #1187 sweep NINA would have
  failed cleanly rather than moved.
- NINA weights points by the HFR spread across stars, which a few
  near-identical fragments would turn into a huge weight. Hocus Focus
  floors that spread at a fraction of the median spread for exactly this
  reason, requires more than one usable star per point, rejects
  outliers with a Grubbs test on weighted residuals, and validates the
  final position with a fresh measurement against the initial HFR.

## Validation

G1 is validated on the Starfront rig with the same ±1200/300 coarse
sweep that failed. At the shipped defaults that sweep leaves three
accepted samples against `min_fit_points` 5, so the pass condition is
that it **refuses** — `not_enough_stars`, the focuser restored to
where it started — rather than converging anywhere, which is itself
the fix for a run that previously reported a confident wrong vertex.
Re-run with `min_fit_points` 3 to exercise the fit: there the run must
end within one grid step of focus — 300 focuser steps on this sweep,
which the 21-step replay of decision 1 sits well inside — with
`confirmed: true`, or fall back to the lowest accepted sample and say
so. A fine ±400/100 sweep must
still confirm at the defaults.

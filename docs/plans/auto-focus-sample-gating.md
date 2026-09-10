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
   `star_count` is the only visible tell. This is the true root and it
   belongs with [#1179](https://github.com/rusty-photon/rusty-photon/issues/1179).
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
| G2 | Hyperbolic V-curve model (`a·√(1 + ((x − c)/b)²)`) for both sweep variants; R² becomes a gate with a threshold knob | Not started | |
| G3 | Measurement-side: aperture HFR around the centroid, two-star minimum per point, hot-pixel and edge rejection — tracked under #1179, needs a real-frame corpus from the rig | Not started | |

G1 first; G2 after G1 because an R² threshold against a parabola rejects
clean fine sweeps (a parabola scores 0.70 on the rig's clean fine sweep,
below the 0.7 NINA uses as its default gate). G3 is independent and must
be validated on real frames, never on synthetic ones — the sweep frames
from the night this was found are still on the rig.

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
sweep that failed: the run must end within one step of focus with
`confirmed: true`, or fall back to the lowest accepted sample and say
so. A fine ±400/100 sweep must still confirm.

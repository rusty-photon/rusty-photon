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
| G3b | Measurement: HFR in an aperture that follows each star (centre and radius iterated, local annulus background), one star per aperture so a fragmented donut counts once, a size cap from `max_area`, and the aggregate over the 20 brightest stars, in `measure_basic` and `measure_stars` ([rp.md](../services/rp.md), `measure_basic` algorithm steps 9–11); `focus-model` reads the new `measure_stars.hfr` — [#1684](https://github.com/rusty-photon/rusty-photon/issues/1684) | Merged | [#1686](https://github.com/rusty-photon/rusty-photon/pull/1686) |
| G3c | Sweep rules, in `focus-model`'s sweep: a fixed star list — a reference frame at each attempt's centre picks the stars, and every sample and the confirmation frame measure those same stars through `measure_stars`' new `stars` input ([rp.md](../services/rp.md), `measure_stars` Contract); a sample with fewer than two of them measured is `sparse`, the fit is weighted by how many measured, `min_star_fraction` retires, and the confirmation says why it was rejected ([focus-model.md § The sweep](../services/focus-model.md#the-sweep)) — [#1684](https://github.com/rusty-photon/rusty-photon/issues/1684) | Merged | [#1698](https://github.com/rusty-photon/rusty-photon/pull/1698) |
| G3d | The retry after a monotonic curve, in `focus-model`'s sweep: when the fit extrapolated its vertex past the end of the samples the lowest one sits nearer, the next grid is centred on that vertex, no further past the end than the samples span, and moves half a width only when the fit placed no such vertex ([focus-model.md § The sweep](../services/focus-model.md#the-sweep), step 6) — [#1684](https://github.com/rusty-photon/rusty-photon/issues/1684) | In progress | `feature/auto-focus-retry-on-vertex` |

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

**Decided 2026-10-31** (owner, on
[#1684](https://github.com/rusty-photon/rusty-photon/issues/1684)'s plan). G3 lands
as G3b, the measurement, then G3c, the sweep rules.

1. **Saturated stars count.** The aggregate keeps them, as
   `measure_basic`'s policy always has: defocused donuts clip in their
   bright annulus.
2. **A confirmation frame below the gate stays a rejection with the
   fallback to the lowest sample.** A sparse frame at the fitted
   position is either a fit far from focus, where stars fade and
   fragment, or a changed sky; either way the measured lowest sample is
   the safer place to end, and a retake only helps against a cloud
   shorter than one exposure. G3c adds the reason to the result,
   `confirmation.rejected: "starless" | "sparse" | "worse" | null`,
   mirroring `curve_points[].rejected`.
3. **The two-star minimum folds into the sparse gate**: its threshold
   becomes `max(2, min_star_fraction × max_stars)` and the reason stays
   `sparse`, so the confirmation frame and the provider's starvation and
   retry logic take it with no change of their own. `min_star_fraction`
   `0` turns off only the relative part.
4. **G3b is validated on the frames already on rig2**, pulled with the
   roof closed: the 2026-09-08 coarse and fine sweeps and the
   2026-09-26 sweeps, 79 frames at known positions.

*Measured in G3b* (the numbers are in
[rp.md](../services/rp.md), `measure_basic`). In apertures, every
sweep that brackets focus keeps its vertex within six steps of the
component HFR's, inside the ~31-step critical focus zone, at an R² of
0.986–0.997; the wing slope is 4.1–4.7 px per 100 steps at both
`threshold_sigma` 8 and 10, where over components it moved from 2.85
to 4.21 with the threshold. The #1187 coarse sweep's wing samples,
which read 4–5 px, measure 23–38 px and lie on the line the fine
sweep's V rises along. The issue's "26–30 px across" for those stars
was wrong: a radial profile puts the disc's edge 45 px from its
centre.

The relative gate was built to reject wing samples that were wrong,
and with honest measurement it rejects some that are right, which
G3b left for the owner:

**Decided 2026-11-01** (owner, after G3b merged). G3c replaces the
sparse gate with a fixed star list in `focus-model`'s sweep: CCDciel's
list, measured at every step, with Hocus Focus's two-star floor per
point in place of CCDciel's rule that a single empty step cancels the
run.

1. **Each attempt measures one list of stars.** Before it walks its
   grid, an attempt takes a reference frame at its centre and keeps the
   stars `measure_stars` finds there whose HFR is within half the
   frame's median HFR of it. That is CCDciel's filter: it drops the
   saturated cores of a frame at focus, which read large, and hot
   pixels and artefacts, which read small. Decision 1 is about a
   frame's aggregate and still holds there; the list is a choice of
   stars. Every sample and the confirmation frame then measure those
   stars and no others, through `measure_stars`' `stars` input: each
   aperture starts at the star's last centre and radius and follows the
   star as in G3b, and a star whose aperture does not settle, or settles
   at an SNR of 3 or less, is not measured in that frame. A sample's HFR
   is the median over the listed stars measured, and its `star_count`
   is how many.
2. **The floor is the gate.** A sample with fewer than two listed stars
   measured is `sparse`, and one with none is starless (`hfr: null`),
   which carries decision 3's two-star minimum over without its
   relative part: `min_star_fraction` retires from `focus-model`. The
   fit is weighted by the listed stars measured.
3. **The confirmation frame measures the list too**, and decision 2
   holds: with fewer than two listed stars measured it is rejected as
   `sparse`, with the fallback, and `confirmation.rejected` says why.
4. **G3c is `focus-model`'s.** `rp`'s capture sweep keeps its relative
   gate unchanged until the focus-model plan's S7 retires it; decision
   3's "both sweeps" goes with it.
5. **Coarse sweeps are not a target.** `focus-model` sizes its sweep to
   the optics, ending it at `sweep.end_ratio`, 4× the focused HFR, and
   this plan declines a coarse-then-fine sweep (Deferred). The #1187
   coarse frames are test data, not a sweep G3c has to win.

*Measured for G3c*, replaying the 79 frames through G3b's measurement:

- Star counts fall as a power of the HFR, because defocus spreads each
  star's light over an area that grows with HFR² and the faint stars
  drop under the threshold: HFR^−1.7 to HFR^−2.5 across the 2026-09
  sweeps at `threshold_sigma 8` / `min_area 6`. A gate at a fraction of
  the densest frame's count is therefore an HFR ceiling — 2.5–4× the
  densest frame's HFR at 10%, where `focus-model` ends its sweep. At
  `min_area 40` the count peaks away from focus: the 2026-09-26 sizing
  sweep's frame at focus holds 7 stars against 35 fifty steps out.
- Listed stars stay measurable where detection loses them. On the
  2026-09-08 fine sweep at `threshold_sigma 8`, 365–497 of its 523
  listed stars measure at every step, where detection finds 44–2,104;
  on the coarse sweep, 148 of 205 still measure 700 steps inside focus,
  where detection finds 10, and 3 measure 1000 steps inside, where it
  finds none (measured by G3c's implementation).
- On the sized sweeps, the 2026-09-26 ±100/25 and ±80/20, every rule
  tried fits at every setting — today's gate, the two-star floor alone,
  Hocus Focus's rules, CCDciel's and the list — with vertices at
  29740–29747, inside the critical focus zone. They differ on the
  confirmation frame. At `threshold_sigma 10` / `min_area 40`, the
  settings the rig ran, the sizing sweep's confirmation frame at 29744
  detects 2 stars, so today's gate rejects it as sparse and the floor
  alone as worse (1.66 px against the lowest sample's 1.05); its six
  listed stars measure 0.93 px, and it confirms.
- On the wider grids, the list keeps every sweep that brackets focus
  fitting at `max_area` 5000. At 500, frames whose stars are larger than
  the cap leave 1–4 sub-pixel artefacts, which the floor alone lets in
  (the 2026-09-08 fine sweep's R² falls to 0.918) and Hocus Focus's
  spread weights favour (its fit moves to 29396, at R² 0.589); the list
  measures none of them, and refuses the #1187 coarse sweep, whose
  wings all lie past the cap, rather than fit it.
  CCDciel's own rule, which cancels a run at its first step with no
  measurable listed star, ends the #1187 coarse sweep, and at
  `max_area` 500 four of the six sweeps that bracket focus.
- No package surveyed gates a sample on its star count relative to the
  sweep (read in their source). N.I.N.A. keeps a starless point with a
  huge error bar and weights by the spread of the stars' HFRs; Hocus
  Focus rejects a point with fewer than two stars and floors that
  spread at 0.2× the sweep's median; KStars recaptures a starless frame;
  CCDciel picks a star list on its first frame and measures it at every
  step, keeping a star above SNR 3.

*Replayed through G3c's implementation* (2026-11-03): each sweep's list
taken on its centre frame by `measure_stars`, every frame then measured
with the list carried, the sweep's own gate and fit. At the rig's
settings (`threshold_sigma 10`, `min_area 40`, `max_area` 5000) every
sweep that brackets focus fits — the 2026-09-08 coarse at 29772 and fine
at 29770, the 2026-09-26 wide at 29747 and 29751, sizing at 29747 and
sized at 29739 — and all three confirmation frames are accepted, the
sizing sweep's on six stars at 0.93 px; the sweeps that walk one wing
end `monotonic_curve`, which shifts. At `threshold_sigma 8` /
`min_area 6` the vertices move by at most 4 steps. At `focus-model`'s
defaults (`threshold_sigma` 5, `min_area` 4, `max_area` 500) the
same sweeps fit within 4 steps of those, but the coarse one, whose wings
lie past the cap, is refused, and the only sub-pixel readings are of
single stars (0.32 and 0.83 px), which the floor rejects. A frame whose
stars straddle the cap is measured by the ones under it and reads low —
the fine sweep's frame at 14.6 px reads 12.3 px on 5 of its 800 stars —
but weighs 5 against the 800 its neighbours measure, and the vertex
holds.

The corpus holds no sized sweep started far from focus, which is the
case the list is for; a deliberately mis-started sized sweep on the
next clear night validates it (Validation).

*Measured on rig2, 2026-11-05*, on the nightly that carries G3c: an L
control at the prediction and three R sweeps started off focus by a
hand-entered offset (R step 22, half width 86, 8 points;
`max_attempts` 2). Every sample of every attempt measured the stars
its reference frame listed at counts within 2 % of the list, every
curve is smooth from 1.2 to 12.9 px, and no fit landed on a sub-pixel
artefact: the list holds. The retry does not reach the plan's range:

| Start (from focus) | Attempt 1 grid | Its extrapolated vertex | Half-width shift to | Outcome |
|-------------------:|:---------------|------------------------:|--------------------:|---------|
| 29770 (+198) | 29684–29838 | 29572 | 29684 | `monotonic_curve` twice, focus outside the second grid too |
| 29371 (−201) | 29285–29439 | 29536 | 29457 | `monotonic_curve` twice, focus outside the second grid too |
| 29721 (+149) | 29635–29789 | 29584 | 29635 | confirmed on attempt 2 at 29572, R² 0.996 |

R's focus was 29572, 17 steps above L's 29555. A shift of half a
width reaches focus on the second attempt only for a miss up to about
two half widths, 163 steps here, while each first attempt had already
extrapolated a vertex within 36 steps of focus and correctly refused
to report it, since it lay outside the samples. Centred on those
vertices, the second grids would have been 29486–29640, 29450–29604
and 29498–29652, each holding 29572. The frames (66, with their
sidecars) join the corpus.

**Decided 2026-11-06** (owner): G3d. A monotonic curve's retry centres
on the vertex the fit extrapolated, capped at the samples' span past
their end, and half a width remains the shift only where the fit
placed no vertex there. `max_attempts` keeps its default of 2: the
capped retry reaches about four half widths from the centre that
failed (about 330 steps for R on rig2), past the plan's 300, where more
attempts of half a width would each cost a whole grid of exposures.

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
   carries no such signal. *Superseded in `focus-model` by G3c
   (decided 2026-11-01):* a sample there is `sparse` below two measured
   stars of the attempt's star list, with no relative part; `rp`'s
   capture sweep keeps this gate until the focus-model plan's S7
   retires it.
2. **Report fit quality, do not enforce it yet.** The result carries
   `fit_r_squared`, the weighted coefficient of determination over the
   accepted samples. A threshold waits for G2.
3. **Confirm the fitted position with a fresh measurement.** After the
   move to `best_position` the tool takes one more frame (one more
   guide sample set on the guiding train) and measures it. The
   confirmation is accepted when the frame has stars, its star count
   passes the same gate as the sweep (in `focus-model` since G3c: two
   of the attempt's listed stars measured), and its HFR is at most
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
  per-point error bars from the HFR spread across stars. G3b's
  apertures give honest per-star HFRs now, but G3c weights by the
  listed stars measured: on the replay that held out the sub-pixel
  artefacts of frames past the size cap, which spread weights favoured.
- A detection fallback for a starless reference frame. An attempt
  whose reference frame shows no stars has an empty list, measures
  every sample as starless and retries on the same grid. Detection at
  every frame could still see stars at one end of the grid and shift
  toward them (focus-model.md, The sweep, step 6), but only when the
  centre lies beyond where any star can be detected — on rig2 between
  700 and 1000 steps from focus (the #1187 coarse sweep detects 10
  stars 700 steps inside focus and none 1000 steps inside), against a
  sized half width of 108. That is a
  broken prediction, and a failed run with the focuser put back is the
  safe answer to it.

## What NINA, Hocus Focus and CCDciel do (for reference)

- NINA's sweep is adaptive: it starts near focus and extends each side
  until it has four trend-line points there. A frame with no stars stays
  in the list with a huge error bar; star count otherwise plays no role.
- NINA validates: default hyperbolic fit, R² threshold 0.7, and a fit
  below it or a vertex outside the sampled range fails the run and
  restores the initial position. On the #1187 sweep NINA would have
  failed cleanly rather than moved.
- NINA weights points by the HFR spread across stars, which a few
  near-identical fragments would turn into a huge weight. Hocus Focus
  floors that spread at 0.2× the sweep's median spread for exactly this
  reason, requires more than one usable star per point, offers a Grubbs
  test on weighted residuals (off by default: its maximum number of
  rejections defaults to 0), and validates the final position with a
  fresh measurement against the initial HFR.
- CCDciel picks a star list on the frame at its starting position,
  dropping stars whose HFD is more than half the median's away from
  it, and measures those positions at every step, following drift; a
  star counts when its SNR, `flux / √(flux + π r² σ²)`, exceeds 3
  (the default). A point is the median HFD of the stars that count, and
  a step where none does cancels the run.
- KStars weights each point by the inverse variance of its stars' HFRs,
  recaptures a starless frame up to three times, and offers an outlier
  pass (Peirce's criterion) and a check of wing points that drop.

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

G3c is validated in two steps. Offline, the 79 rig2 frames replay
through the implementation itself — the reference frame at each
sweep's centre, the list measured at every step — and must reproduce
the replay above: every sized sweep fitting, with the vertices it
found, and the 2026-09-26 sizing sweep's confirmation frame accepted at
`threshold_sigma 10` / `min_area 40`. Done 2026-11-03 (*Replayed
through G3c's implementation*, above); the unit tests carry the
recorded sweeps as the list measured them. On the next clear night, a sized
sweep started 150–300 steps from focus, deliberately, must either fit
or shift toward focus and fit on its second attempt, and must never
fit on sub-pixel artefacts; the frames join the corpus. Run on rig2 on
2026-11-05 (*Measured on rig2*, above): the list and the floor passed,
150 steps passed, and 200 failed on either side through the
half-width retry, which G3d replaces. G3d is validated the same way:
on the next clear night the ±200 mis-starts, and a +300 one, must fit
on their second attempt.

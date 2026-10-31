# `rp-focus-curve` Crate Design

The V-curve a star traces through focus, and its weighted fit. Two sweeps
fit it: the `focus-model` provider's capture sweep
([focus-model.md § The sweep](../services/focus-model.md#the-sweep), step 4)
and `rp`'s guide-train sweep, which measures PHD2's HFD instead of
capturing ([rp.md § Guide-train sweep](../services/rp.md#guide-train-sweep-phd2-metric-variant)).
One fit in one place means the two sweeps cannot drift apart in how they
find focus.

This is a workspace library, not a service. It depends on `thiserror` and
nothing else.

## Scope

- The hyperbolic model and its value at a position.
- The weighted least-squares fit of the model to `(position, value, weight)`
  samples, and the fit's weighted coefficient of determination.

Out of scope: everything a sweep decides around the fit. That covers the
sparse gate, the bound on the centre, a threshold on the R², the
confirmation frame and the retry. The two sweeps decide these differently:
the provider bounds the centre by the accepted samples and may enforce
`min_fit_r_squared`, while `rp` bounds it by the grid and enforces nothing.
Rounding the centre to a whole focuser step also belongs to the caller,
next to that bound, so the crate returns it as an `f64`.

## The model

```
value(x) = a·√(1 + ((x − x₀)/b)²)  =  √(a² + m²·(x − x₀)²),   m = a/b
```

- `x₀` (`Hyperbola::centre`): best focus, in focuser steps.
- `a ≥ 0` (`Hyperbola::minimum`), the value at the centre: the focused
  HFR (or HFD) that seeing and the optics leave.
- `m` (`Hyperbola::slope`), the wing slope, in value units per step.
  Far from focus the curve
  is the two lines `m·|x − x₀|`. Geometrically this is the cone of a
  defocused star, whose half-flux radius grows by the slope that
  [`focus-model`'s sweep sizing](../services/focus-model.md#sweep-sizing)
  predicts from the optics.
- `b = a/m`, the defocus at which the value has grown by `√2`.

`a = 0` is the limiting V, two straight lines meeting at `x₀`. The family
reaches it continuously: `b` goes to 0 while `m` stays finite, which is
why the crate carries the slope rather than `b`, and reports a limiting V
as a `minimum` of 0 beside its slope.

The hyperbola is the right model because it *is* the defocus curve: a
cone with a seeing floor. The choice rests on physics that holds for
every optical train, not on one rig's numbers
([sample-gating plan](../plans/auto-focus-sample-gating.md), G2). A
parabola has the right minimum and the wrong wings: it bends where a
defocused star grows in a straight line. The wider a sweep reaches, the
further a parabola's vertex is pulled off focus by the wing on the longer
side.

## The fit

The fit minimises the weighted residuals in the value itself,
`Σ wᵢ·(yᵢ − value(xᵢ))²`, where the caller's weight is the sample's
star count (or valid frame count).

1. **Samples.** A sample whose weight is zero is dropped. Fewer than three
   left is `NotEnoughStars`.
2. **Frame.** Positions are recentred on the weighted mean and scaled by
   half their span. At real focuser scales (positions in the tens of
   thousands, a sweep a few hundred steps across) the sums would
   otherwise lose most of their precision.
3. **Start.** The square of the hyperbola is a parabola,
   `y² = A + K·(u − u₀)²` with `A = a²` and `K = m²`, so a weighted
   linear least-squares fit of the squared values gives a starting `u₀`,
   `A` and `K` in closed form.
   - Samples at fewer than three positions are `MonotonicCurve` before
     any sum is taken, and so is a design matrix still too close to
     singular, tested against a relative scale and never against zero.
   - A curvature `K` at or below `1e-12` of the mean squared value is
     also `MonotonicCurve`: the squared values do not turn upward, so
     there is no minimum to find. The scale matters because rounding
     leaves a flat curve a curvature of `1e-20` or so, not an exact
     zero.
   - A negative `A`, where the squared curve dips below zero, starts at
     `A = 0`.
4. **Refinement.** Levenberg–Marquardt on `(u₀, A, K)` starts from there,
   holding `A` and `K` at zero or above. A step is taken only if it lowers
   the weighted residual, so the result is never worse than the start.
   The refinement stops at the first of four conditions:
   - a step improves the residual by less than a relative `1e-12`;
   - the residual reaches 0;
   - no damped step improves it;
   - 100 iterations have run.

   A refinement that flattens the curve (`K` falls to that same scale)
   is `MonotonicCurve`: a constant has no centre.

The start alone would weight each residual by roughly the square of its
value, so the wings would dominate. The wings are where a fragmenting
detector puts its worst samples
([#1187](https://github.com/rusty-photon/rusty-photon/issues/1187)), so
the refinement in the value itself is the fit; the start is only where
it begins. On every noisy generated curve of the crate's tests, the
refinement ends at the residual a brute-force scan of the parameters
finds.

### Three samples

Three samples meet three parameters. When a hyperbola passes through
them, the fit is that hyperbola, at R² 1. When none does, the
fit is the nearest V, `a` 0, at an R² below 1. That happens when the
wings are steeper than any rounded floor allows. It is a fit and not an
error, because it is exactly what a coarse grid measures: every sample
sits far enough from focus that the floor never shows. The #1187 coarse
sweep's three dense samples admit no hyperbola, and their nearest V
centres 4 steps from focus.

## R²

The weighted coefficient of determination over the fitted samples,
`1 − Σwᵢ(yᵢ − value(xᵢ))² / Σwᵢ(yᵢ − ȳ)²` with `ȳ` the weighted mean, is
clamped to `[0, 1]`. Samples whose values are all equal have no spread
to explain and score 0 (they never get this far: their squares do not
turn upward). The clamp is a requirement and not a convenience: a
threshold of `0` has to mean "accept every fit", and a raw coefficient
goes negative on a fit worse than the mean
([sample-gating plan](../plans/auto-focus-sample-gating.md), G2).

## Errors

| Variant | Message |
|---------|---------|
| `NotEnoughStars { got }` | `not enough stars: only 2 of the 3 samples a fit needs carry any weight` |
| `MonotonicCurve(why)` | `monotonic curve: ` and then the reason: `the samples sit at 2 positions, and a curve needs 3`, `design matrix is singular (det=…, scale=…)`, `the squared values do not turn upward (curvature -1.200e-3)`, or `the fitted curve flattened to a constant, which has no centre` |

The provider folds `NotEnoughStars` into its own error of that name, with
the 3 as the samples needed, so its run records read the same as they
did with the parabola. `rp` relays the message as it stands.

## Consumers

- `focus-model` (`sweep.rs`): gates, fits, refuses a centre outside the
  accepted samples as `monotonic_curve`, then applies
  `trains.<id>.min_fit_r_squared` as `poor_fit`.
- `rp` (the guide-train branch of `auto_focus`): fits, refuses a centre
  outside the grid as `monotonic_curve`, and reports the R² with no
  threshold.

`rp`'s capture sweep keeps its own parabola until the focus-model plan's
S7 retires it.

## Testing

The crate's unit tests cover:

- exact recovery of a hyperbola at real focuser scales, and of the
  limiting V;
- the three-sample rule both ways;
- the flat, concave, single-position and too-few refusals;
- zero weights dropped, and heavy weights pulling the fit;
- samples on one wing extrapolating the centre past their end, which is
  what the callers' bound exists for;
- the value at a position, and an R² inside `[0, 1]` on every noisy
  curve;
- noisy curves on which the refinement ends at the residual a
  brute-force scan finds.

The sweeps' own tests carry the curves generated from optics and the
rig's recorded sweeps. They live in focus-model's `sweep.rs`, which
sizes its grids as the provider does.

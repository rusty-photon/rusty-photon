# Plan: `focus-model` — a tool provider that is the expert at focusing a train

## Goal

`auto_focus` runs one V sweep around wherever the focuser happens to be
and knows nothing about why it is there, how wide the sweep should be
for the optics in front of it, or what the last sweep on this train
found. Nothing in the system remembers where a train last focused, how
far each filter sits from the others, or how the focus position moves
with temperature, so every refocus starts from the previous result plus
whatever drifted since, and a filter change or a cold front puts the
start outside the band the detector can measure. The night-3 coarse
sweep on the Starfront rig
([#1187](https://github.com/rusty-photon/rusty-photon/issues/1187))
failed for exactly that reason: a ±1200-step grid that reached three
times beyond the measurable band, run because nobody knew how far off
the start was or how wide the band is for that telescope.

Every mainstream package solves this the same way. It does not widen
the sweep; it sizes the sweep to the optics, corrects the *start* with
per-filter offsets and a temperature model, retries a failed sweep with
the same parameters after restoring the start, and shifts the range when
the minimum is not bracketed. The packages also agree on where the
knowledge lives: the core owns the focuser, its backlash and star
measurement, and the autofocus algorithm is a component on top of them.
This plan brings that shape to Rusty Photon in two layers:

- **`rp` owns the physics and the measurements.** Moving a focuser to a
  position that lands on the same mechanical place every time is a
  hardware problem, and ASCOM does not standardise backlash the way
  INDI does, so `rp`'s `move_focuser` compensates it for every driver
  ([#1201](https://github.com/rusty-photon/rusty-photon/pull/1201)).
  Capturing a frame and measuring its stars are needed by more than
  focus. Which focusers a train shares, what its focal length,
  aperture, pixel size and step size are, and which filter is in the
  path are facts of the rig, and `rp` is the only owner of the train
  model. `rp` also keeps the event vocabulary a night is watched
  through, and emits the `temperature_changed` event its events table
  promised ([#1203](https://github.com/rusty-photon/rusty-photon/issues/1203)).
- **A provider owns the expertise.** `focus-model` is a first-party
  tool provider shaped like `calibrator-flats`: a store keyed by train,
  tools served through `rp`'s catalog, the rig driven by calling `rp`
  back. It knows how to reach focus on an optical train that is roughly
  in focus already: it sizes the sweep from the critical focus zone of
  the optics at the filter's wavelength, walks the V through `rp`'s
  primitives, gates and fits the samples, confirms the vertex, retries
  and shifts when the minimum was not bracketed. On top of that core it
  holds each train's reference filter, per-filter offsets, temperature
  coefficient and last good focus, predicts where a sweep should start,
  and learns from every result
  ([#1204](https://github.com/rusty-photon/rusty-photon/issues/1204)).
  Those are optimisations: they reach the optimum faster, and focus is
  reached without them. Triggers stay in the session document, where an
  operator started them (tenet 3).

The outcome: a filter change or a temperature swing moves the focuser
to a predicted position before the sweep, the sweep is sized from the
telescope rather than guessed, a failed sweep retries the way N.I.N.A.
and Ekos do, and the numbers that make all of that work are learned
once and kept.

## Implementation Status

| Slice | Description | Status | Issue / PR |
|-------|-------------|--------|------------|
| S1 | `rp`: `auto_focus` retries with the same parameters, shifts on `monotonic_curve`, reports the wing slope, carries `curve_points` in fit-failure errors | Merged | [#1202](https://github.com/rusty-photon/rusty-photon/issues/1202), [#1210](https://github.com/rusty-photon/rusty-photon/pull/1210) |
| S2 | `rp`: `temperature_changed` emitted from the focuser probes on a delta; `session-runner`: `refocus-on-temperature` rule in `deep_sky.json` | Merged | [#1203](https://github.com/rusty-photon/rusty-photon/issues/1203), [#1209](https://github.com/rusty-photon/rusty-photon/pull/1209) |
| S3 | `rp`: the optical facts on the train model (`aperture_mm`, filter wavelengths, `microns_per_step`) and `get_train_info.optics`; `get_refocus_plan`; the `focus_tools` registration declaration with the focus event bracket | Merged | [#1214](https://github.com/rusty-photon/rusty-photon/pull/1214) |
| S4 | `focus-model`: crate, store, server, doctor, packaging, registration; the sweep; `focus_train`, `get_focus_model`, `get_focus_runs`, `set_focus_offsets`, `reset_focus_model`, `get_sweep_plan`; `rp`: `get_focuser_position` reports the focuser's bounds | Merged | [#1204](https://github.com/rusty-photon/rusty-photon/issues/1204), [#1215](https://github.com/rusty-photon/rusty-photon/pull/1215) |
| S5 | `focus-model`: `determine_filter_offsets` | Merged | [#1204](https://github.com/rusty-photon/rusty-photon/issues/1204), [#1231](https://github.com/rusty-photon/rusty-photon/pull/1231) |
| S6 | `focus-model`: `calibrate_temperature` | Merged | [#1204](https://github.com/rusty-photon/rusty-photon/issues/1204), [#1240](https://github.com/rusty-photon/rusty-photon/pull/1240) |
| S7 | `session-runner`: `deep_sky.json` calls `focus_train` everywhere it called `auto_focus` and `refocus_train`; `rp`: the capture-based `auto_focus` and `refocus_train` retire, the imaging train's `auto_focus` block goes with them, `rp.md`'s invalidation table stops saying "backlog" | Not started | |
| S8 | `rp`: `optical_trains[].obstruction_mm` with its load-time bounds, and `obstruction_mm` + the derived `obstruction_ratio` on `get_train_info.optics`; `focus-model`: D9 derives the blur constant from it, and the record identity grows the optical facts (O1, O8) | Not started | |
| S9 | `focus-model`: the hyperbolic fit replaces the ported parabola, `min_fit_r_squared` reported and not enforced by default; `rp`: the same model in the guide-metric sweep (gating plan G2) | Not started | |
| S10 | = [device-claims-and-phd2-camera.md](device-claims-and-phd2-camera.md) C7: the guiding train captured through the PHD2 camera facade (its C6) with PHD2 `Stopped`, under that plan's sweep, train and mount-motion leases; `focus-model`: the purpose guard, then `focus_train` and `determine_filter_offsets` accept a guiding train under O4's rules 1–7 (its own focuser, the facade as its camera; the imaging-first order is the provider's only inside a `shared: true` walk on the guiding train, the caller's otherwise — O4 rules 2–3) | Blocked on the S7/D17 ↔ C7 reconciliation both plans record | |
| S11 | `focus-model`: `trains.<id>.temperature_sensor` names a stand-in focuser probe for a train whose own focuser has none, with the sensor id in the record identity; prediction-only, the refocus trigger is unchanged (O3) | Not started | |

S3 to S6 are merged. S7 waits for a rig night on S4.

S8 to S11 come out of the 2026-09-13 review of the open items below,
ordered by what they unblock: S8 is `rp` reads plus a sizing change;
S9 lands the fit model, and the measurement work behind
[#1179](https://github.com/rusty-photon/rusty-photon/issues/1179)
(gating plan G3) follows it, because the hyperbola's wings are fitted
to exactly the samples that work makes honest; S10 reaches furthest,
since it is device-claims-and-phd2-camera.md's C7 and rides on that
plan's facade (C6); S11 is provider-only. Each
slice follows [development-workflow.md](../skills/development-workflow.md):
design-doc update first (rp.md, session-runner.md, a new
`docs/services/focus-model.md` for S4), BDD second, code third.

## Background

### What exists

- `auto_focus` (rp.md § `auto_focus` Contract): a symmetric grid of
  `half_width` each side of the current position in `step_size`
  increments, walked in the focuser's backlash approach direction,
  weighted parabola fit over the samples that pass the sparse gate, a
  confirmation frame at the vertex with fallback to the lowest accepted
  sample, a restore to the start on `not_enough_stars` or
  `monotonic_curve`, and with S1 a bounded retry with the same
  parameters, a shift toward the lowest accepted sample on a monotonic
  curve, the wing slope on the result and `curve_points` in fit-failure
  errors. Per-train parameters live in the train's `auto_focus` block
  (`config/optical_train.rs`). The result carries `temperature_c` from
  the terminal focuser's probe. The guiding train has the PHD2-metric
  variant of the same sweep, which reads the guider's star metric
  instead of capturing frames.
- `refocus_train`: expands one trigger into the train model's
  dependency-ordered sequence when focusers are shared, and pauses
  guide corrections around the capture-based steps that move a
  guide-coupled focuser.
- `move_focuser` with `focusers[].backlash`: approach-direction
  compensation for every move, and a settle rule of idle plus read-back
  equals target. A position commanded from either side lands on the
  same mechanical place, which is what makes a predicted start land
  where the sweep's samples were measured.
- `capture` and `measure_stars`: a frame into the document store, and
  per-star HFR, FWHM and flux with `star_count` and medians over it.
  Guiding is paused and resumed with `pause_guiding` /
  `resume_guiding`.
- `get_train_info`: the terminal camera, the sole filter wheel with its
  filter names, the focusers in optical order and the terminal focuser,
  the focal length. Pixel size is read from the camera and used for the
  pixel-scale derivation. When this plan was written nothing knew the
  aperture, the filters' wavelengths or the focuser's microns per
  step; S3 shipped all three as `optics` and `filter_wavelengths_nm`
  (D14), and this bullet describes the starting point, not today.
- Focus events: `focus_started`, `focus_complete` and `focus_failed`
  are rendered by `ui-htmx`'s stream page, counted by the deep-sky BDD,
  and re-arm the Guide Focus Watch's baseline when a sweep moved a
  guiding-train focuser.
- `deep_sky.json` refocuses after N frames, on HFR degradation against
  the last `final_hfr`, on the guide-focus watch's events, and with S2
  on a `temperature_changed` delta for the imaging train's terminal
  focuser. `rp` emits that event from a slow poll of every connected
  focuser's probe.
- `rp.md`'s derivation table answers "what does a filter change on
  wheel W invalidate?" with "focus offset of trains containing W".
  It said "(per-filter offsets: backlog)" when this plan was written;
  the offsets shipped with S5, and this branch updates that row.
- `calibrator-flats` ([calibrator-flats.md](../services/calibrator-flats.md),
  [plan](archive/calibrator-flats-provider.md)): the first first-party
  tool provider. Its shape — redb store keyed by train and filter,
  staleness that names the field that changed, `rp` dialled lazily per
  tool call, nothing moved by a bad request, everything put back on
  exit — is the template for this provider.

### What the packages do

Surveyed 2026-09-09 across N.I.N.A. and Hocus Focus, Sequence Generator
Pro, KStars/Ekos, FocusMax, TheSkyX @Focus2/3, Voyager, ASIAIR, APT,
SharpCap, MaxIm DL and CCDAutoPilot
([N.I.N.A.](https://nighttime-imaging.eu/docs/master/site/advanced/autofocus/),
[Hocus Focus](https://ghilios.github.io/hocus-focus/overview/autofocus/),
[SGP](https://help.sequencegeneratorpro.com/beta/Focusing/Auto%20Focus/UnderstandingAutoFocus.html),
[Ekos](https://kstars-docs.kde.org/en/user_manual/ekos-focus.html),
[Voyager](https://wiki.starkeeper.it/index.php/AutoFocus_Setup),
[APT](https://astrophotography.app/usersguide/auto_focusing_aid.htm),
[SharpCap](https://docs.sharpcap.co.uk/4.1/6_GettingGoodImages.htm),
[MaxIm DL](https://cdn.diffractionlimited.com/help/maximdl/Autofocus_Tutorial.htm)):

- **One symmetric V, sized to the optics.** Move out about half the
  span, step inward through focus. SGP puts the sweep ends at 3–5× the
  focused HFR, Ekos's Focus Advisor at 2–3×, N.I.N.A. at 80 % of the
  range over which HFR is still detectable, APT at 1.5–2 critical focus
  zones per step. Only MaxIm DL runs coarse-then-fine as its normal
  path.
- **Adaptive when the minimum is not bracketed.** N.I.N.A. and Hocus
  Focus keep stepping until enough valid points flank the minimum; SGP
  Smart Focus shifts the range; Ekos restarts from a shifted start, at
  most three times.
- **Restore and retry with the same parameters on failure.** N.I.N.A. up
  to a configured number of attempts, Ekos one rerun on low R², Voyager
  and FocusMax bounded retries. No package halves or doubles the step
  on failure; step changes only appear as designed second passes.
- **Drift is handled by moving the start, never by widening the sweep.**
  Per-filter offsets (every package), temperature-triggered refocus
  (N.I.N.A., SGP, Voyager), a temperature model that adapts the start
  or the position between frames (Ekos Adapt Start Pos and Adaptive
  Focus), a start recomputed from the last focus (FocusMax).
- **Offsets are determined the same way everywhere:** focus the
  reference filter, then each other filter, over a few rounds; the
  offset is the typical difference.
- **The algorithm is a component, the focuser and the measurement are
  the core.** N.I.N.A.'s core moves the focuser with backlash
  compensation and detects stars, and Hocus Focus replaces the
  autofocus algorithm behind an interface. Ekos's focus module owns the
  algorithm while INDI drivers own backlash. Star measurement is shared
  with sequencing and image grading in both.

### The numbers on the rig

Night 3 on the Starfront rig, focus at 29766: HFR 1.0 px at focus, 4 px
at 100 steps, 8 at 200, 17 at 400; between 600 and 1200 steps the field
holds 0–12 stars and the samples are fragment artefacts. By the SGP and
Ekos rules the sweep for that optic is a step of 25–30 with a half
width of 100–125. The fine block in use (±400 by 100) reaches 17× the
minimum at its far end and fits only because the gate and weights carry
it; the coarse grid (±1200 by 300) never could. The measured slope, 4 px
per 100 steps, is what the blur geometry in D9 predicts for that
telescope's focal ratio, pixel size and focuser step, so the sweep can
be sized from the optics before the first frame. A predicted start
within a few tens of steps of focus is what makes a properly sized
sweep usable at all.

### The gaps this plan closes

1. No knowledge of the optics: the sweep is sized by hand per train,
   and nothing checks the hand-entered block against the telescope.
2. No memory of focus: nothing knows a train's last good position, its
   filter offsets or its temperature behaviour.
3. No start correction: a filter change or a temperature swing is
   absorbed by the sweep, or not at all.
4. The focus expertise lives in `rp`'s process, where it cannot evolve
   without a change to the observatory service, next to primitives
   that every other consumer shares.

## Decisions

### D1 — `rp` owns the physics and the measurements; the provider owns the expertise

The line between the two is the one N.I.N.A. and Ekos draw. `rp`
keeps, as built-ins:

- `move_focuser` with backlash compensation and the settle rule, and
  `get_focuser_position` / `get_focuser_temperature`;
- `capture` and `measure_stars`, and the document store they write;
- the train model: `get_train_info` with the optical facts of D14, and
  `get_refocus_plan` of D16;
- `set_filter`, `pause_guiding` / `resume_guiding`, and the guider's
  metric stream;
- the focus event triple, emitted around a provider's focus tool (D15);
- the PHD2-metric sweep of the guiding train. O4 keeps it there for
  the in-session case permanently; only a training run moves to the
  provider's capture sweep through the PHD2 camera facade (S10,
  device-claims plan C7).

The provider owns everything that is *knowing how to focus*: sizing the
sweep from the optics (D9), walking the V, gating and fitting the
samples, confirming the vertex, retrying and shifting (D13), the memory
of past focus and the predicted start (D3–D8), and the offsets and
temperature procedures (D7, D8). The provider never moves anything
except through `rp`'s tools, and `rp` never fits a curve for an imaging
train once S7 lands (D17).

The provider is `focus-model`: crate `services/focus-model`, binary
and unit `rusty-photon-focus-model`, config `focus-model.json`, store
`focus-model.redb`, port 11173 (next to `calibrator-flats` 11170,
`session-runner` 11171, `polar-align` 11172), MSI feature `FocusModel`.
The name is what the provider owns and what `get_focus_model` returns,
the way `calibrator-flats` is named for the flat training it returns.

### D2 — MCP only, dialled lazily, nothing moved until asked

As `calibrator-flats` D2 and D10: `/mcp` and `/health` share the
`server` block; `rp` registers the provider as a `tool_provider` and
proxies its tools; the provider answers `tools/list` with no `rp` in
sight and connects to `rp` per tool call with the observatory
credential ([ADR-017](../decisions/017-standard-mcp-client-construction.md));
`rp`'s packaged unit orders itself after this one. No run surface, no
UI, no events of its own. Tenet 3: nothing this provider does at
startup, on config load or on a health probe touches a device.

### D3 — The store: one record per train

One redb file with the `rp-targets` conventions
([rp-targets.md](../crates/rp-targets.md)): a `meta` table with
`schema_version`, serde-tolerant records, a refusal to open a newer
build's file. Path: `store_path` when set, else the platform state
directory (`/var/lib/rusty-photon/focus-model/` on Linux, the packaged
unit's `StateDirectory=`).

- **Key:** train id. Offsets are per filter *inside* the record, because
  the offsets, the reference and the temperature model describe one
  optical train together and go stale together.
- **Record:** `train_id`, `focuser_id`, `camera_id`, `filters` (the
  wheel's names at write time, or null), `reference_filter` (or null),
  `offsets` (filter name → steps relative to the reference; the
  reference maps to 0), `temperature_coefficient` (steps per °C, or
  null) with `coefficient_runs` and `coefficient_span_c`, `last_good`
  (a list, one entry per filter — `filter` null on a filterless train —
  of `{position, temperature_c, hfr, at}`, the most recent confirmed
  result on that filter), and `runs`, the most recent `runs_kept`
  focus runs (config, default 500), newest last.
- **A run is the whole measurement.** `{at, filter, outcome, error,
  position, hfr, best_position, best_hfr, fit_r_squared, samples_used,
  attempts, wing_slope, temperature_c, step_size, half_width,
  sweep_source, prediction, curve_points}`, the curve points being
  `{position, hfr, star_count, document_id, rejected}` exactly as the
  sweep measured them. The points are the measurement; the slope and
  the fit are derived from them. Neither the hyperbolic model nor the
  blur constant is *chosen* from these records: G2's model is analytic
  and validated across generated curves, with recorded sweeps kept as
  regression cases, and since O1 the blur constant is derived from the
  train's obstruction. The records validate both; they select
  neither (D5, D9, gating plan G2). A failed run is recorded too, with `outcome` naming the
  sweep's error (`not_enough_stars`, `monotonic_curve`, `poor_fit`
  once S9 lands the threshold, `cancelled`,
  or `error` with the text in `error`) and null where it measured
  nothing: the run an operator wants to see the morning after is the
  one that failed. `confirmed` and `fallback` are the two successful
  outcomes.
- **Nothing ages out by time.** An old focus position is still a
  measured position, and the temperature term is what corrects for the
  time since. The cap is a count: 500 runs is a season, and with points
  it is under a megabyte per train; the coefficient (D8) only becomes
  good across seasons. What invalidates a record is a physical change
  — a camera or focuser swap the identity fields see, a re-homed or
  re-seated focuser they cannot.
- **Stale means unknown, and a stale record resets on its next
  write.** A record whose `focuser_id`, `camera_id` or filter-name set
  no longer matches `get_train_info` is reported stale naming the
  field, as flats names a changed camera field; S8 adds the optical
  facts D9 reads to that identity (O8) and S11 the temperature sensor
  (O3). A stale record
  predicts nothing: `focus_train` sweeps from the current position,
  then writes a fresh record holding that run alone and reports
  `model: "reset: camera_id changed from a to b"`; `set_focus_offsets`
  and `determine_filter_offsets` do the same with what they write.
  Until that write `get_focus_model` shows the old record with the
  stale field, so the operator can copy its offsets first. No run
  measured through a different camera ever feeds a fit.
- **`reset_focus_model {train_id}`** is the operator's answer to the
  change the identity fields cannot see. It drops the runs, `last_good`
  and the coefficient and keeps the reference and the offsets, which
  are differences between filters and survive a re-home; the result
  names what was dropped and what was kept. The alternative is
  deleting a redb file on the rig, which is the wrong interface.
- **Writes are the tools' own.** `focus_train` appends a run and
  updates the filter's `last_good` entry on a confirmed result;
  `determine_filter_offsets` writes the reference, the offsets and
  `last_good`; `calibrate_temperature` writes the coefficient;
  `set_focus_offsets` writes the reference and offsets;
  `reset_focus_model` drops. `get_focus_model`, `get_focus_runs` and
  `get_sweep_plan` never write.
- **What stays out.** Frames belong to `rp`'s document store under its
  own eviction policy; a curve point carries the frame's `document_id`
  as the cross-reference for as long as the FITS sits on disk. The
  session document's refocus-on-temperature baseline stays in the
  session, and `rp`'s temperature watch baseline stays per device
  handle. The store is the only cross-session memory of focus, one
  provider per `rp`. The guiding train gets a record only once its
  sweep moves into the provider under O4.

### D4 — The predicted start

`focus_train` computes the start position from the record and moves the
focuser there before the sweep:

```
start = last_good.position
      − offset(last_good.filter)
      + offset(filter)
      + coefficient × (temperature_now − last_good.temperature_c)
```

`last_good` is the most recent entry of the record's per-filter list
(D3). Every term the record cannot supply is omitted: no `last_good`
means no prediction and the sweep starts where the focuser is. When
the anchor entry is on another filter and either offset is missing,
the prediction falls back to the target filter's own `last_good`
entry, whose offset terms cancel; without one there is no prediction,
reported as `prediction.missing: ["offset"]` — a narrowband position
is a worse start for a luminance sweep than wherever the focuser sits.
No coefficient or no temperature reading means no temperature term,
reported the same way. `temperature_now` is `get_focuser_temperature` on the
train's terminal focuser — or on the focuser `trains.<id>.temperature_sensor`
names, where the train's own has no probe (O3, S11) — read once at the start
of the call. The move
is one `move_focuser`, so the backlash rules apply and the sweep's
samples and the predicted start are approached from the same side. A
prediction closer to the current position than half a critical focus
zone (D9) is not moved to, because the two positions are the same
focus; when the optics are unknown the threshold is
`min_prediction_move` steps (config, default 5). A prediction
outside the focuser's bounds — which `get_focuser_position` reports
beside the position — is not moved to either; the sweep runs from the
current position and `prediction.skipped` says why.

### D5 — What a run teaches

After the sweep the provider appends a run to the record (D3): the
outcome, the position and HFR it settled on, the fit, the sweep's
`step_size` and `half_width`, the prediction it made, the filter and
the temperature it read, and every curve point. Only a **confirmed**
result updates the filter's `last_good` entry: a fallback result is a
measured position but not a trusted fit, and the session's own
refinement sweep is the place it gets confirmed. The wing slope
**validates** D9's blur constant for this train; since O1 it is
derived from the train's obstruction and is never fitted from the
slope, so a mismatch is evidence about the configured optics, not a
constant to re-learn.

### D6 — Put things back

If the sweep fails after every attempt, the provider moves the focuser
back to the position it read at the start of the call, which is where
the rig was before the call, not the predicted start, and returns the
sweep's error with the prediction it made and the final attempt's
`curve_points`. Cancellation from `rp` — the unsafe transition, or the
caller going away — is observed between primitive calls and followed
by the same put-back. Once S10 lands, a training run's put-back also
releases the sweep, train and mount-motion leases the run held
(O4, device-claims plan D12) on the failure and cancellation paths
too; PHD2 was `Stopped` throughout and the run restarts nothing. `determine_filter_offsets` restores the filter
that was selected before the call and leaves the focuser at that
filter's measured position from the last completed round, which is a
measured place, never a computed one.

### D7 — Determining offsets

`determine_filter_offsets {train_id, filters?, reference?, rounds?}`,
the procedure every package documents:

1. Resolve the wheel from `get_train_info`; a train without exactly one
   filter wheel is an error naming the train. `filters` defaults to the
   wheel's list, `reference` to the stored reference, else the first
   filter of the wheel; `rounds` defaults to 2, at most 5.
2. Per round: focus the reference filter, then each other filter. The
   sweeps are the provider's own, not `focus_train` calls back through
   `rp` — the amendment D16 made for the shared walk, and here for a
   second reason: the procedure holds the one-run-at-a-time claim for
   its whole length, so a call that reached its own tool through `rp`
   would wait on a claim it is holding itself. Each sweep is recorded
   as a run and carries its own guiding handshake; the procedure does
   not hold one across the wheel, because ten sweeps is half an hour of
   an uncorrected mount. A filter's difference in a round is its
   confirmed position minus the round's confirmed reference position; a
   round in which either sweep was not confirmed contributes nothing
   for that filter, and a round whose reference did not confirm
   contributes nothing at all. A sweep that fails to fit is one of
   those; a device error or a cancellation ends the procedure.
3. A filter's offset is the median of its differences over the rounds,
   the mean of the middle two rounded away from zero on an even split,
   an offset being whole steps. A filter with no usable round has no
   offset and is named in the result; the others are written. When no
   filter has one the call is an error, the runs it recorded standing
   as the account of why. The reference and the offsets are this
   call's own write, made once at the end; every sweep has already
   appended its run and, where it confirmed, updated its own filter's
   `last_good` entry (D3) — the reference filter's and every other
   one's alike, since each sweep is the body `focus_train` runs.

The tool reports every sweep's position, HFR and confirmation per round
so an operator can see the spread. Temperature drift inside a round is
bounded by the round's length and is not modelled; the reference is
refocused every round for that reason.

### D8 — The temperature model

`calibrate_temperature {train_id}` fits `position − offset(filter)`
against `temperature_c` by least squares over the record's confirmed
runs that carry both. It refuses, naming the threshold, with fewer than
`min_calibration_runs` (config, default 5) or a temperature span below
`min_calibration_span_c` (default 3.0), and writes the coefficient with
the run count and span otherwise. The coefficient is used only in D4's
start prediction. Moving the focuser between frames as the temperature
drifts, the Ekos Adaptive Focus behaviour, is O5.

The offset term is what puts two filters on one scale, and it is only
needed when the fit mixes them: a constant shifts the line without
tilting it, so runs all taken through one filter fit without any
offset at all, and a train that never ran D7 still gets a coefficient.
A run whose filter the record holds no offset for, in a set that spans
several, cannot be placed against the rest; it is left out and
counted. What the fit subtracted is stored with the coefficient, and a
later write that moves one of those offsets drops the coefficient:
the number would otherwise describe a scale the record no longer
keeps. The runs stay, so re-fitting is one call. The call reports the scatter as `residual_steps`, the root
mean square of the runs about the fitted line, because a coefficient
with no measure of its spread is a number an operator cannot judge.

A stale record is refused rather than fitted. No run measured through
another camera or focuser describes this rig (D3), and a fit is
exactly the place that would launder them into one number. The call
takes the provider's one-run claim, as every other write does: a sweep
in flight is about to append the run the fit wants, and D7 is about to
write the offsets it scales with.

### D9 — Sizing the sweep from the optics

The provider derives the sweep from the train's optical facts (D14)
and the filter's wavelength, before any frame is taken:

```
N          = focal_length_mm / aperture_mm                 focal ratio
ε          = obstruction_mm / aperture_mm, 0 if none      `optics.obstruction_ratio`; null without an aperture, which N needs too (D14)
c          = 0.5 × sqrt((1 + ε²) / 2)                      half-flux radius of the annular blur
λ_um       = wavelength_nm / 1000                          the filter's wavelength (D14)
CFZ_um     = 4.88 × λ_um × N²                              critical focus zone
cfz_steps  = CFZ_um / microns_per_step
slope      = c × microns_per_step / (N × pixel_size_um)    HFR growth, px per step
hfr_focus  = last_good.hfr, else 0.5 × seeing_fwhm_arcsec / pixel_scale_arcsec_per_pixel
half_width = ceil(hfr_focus × sqrt(end_ratio² − 1) / slope)
step_size  = max(ceil(2 × half_width / (points − 1)), ceil(cfz_steps / 2))
```

The half width places the sweep ends at `end_ratio` times the focused
HFR (config, default 4.0, the middle of the SGP and Ekos 3–5× band),
using the geometric growth of a defocused star: a defocus of Δ microns
along the axis is a blur circle of Δ/N across, and the half-flux
radius of that disc is `c` times Δ/N. `c` follows from the central
obstruction and nothing else: half the flux of an annulus of outer
radius R and obstruction ratio ε falls inside R·√((1+ε²)/2), so
`c` = 0.5·√((1+ε²)/2) — 0.3536 unobstructed, 0.3734 for an SCT at
ε = 0.34, and 0.5 in the limit. A train that configures no
`obstruction_mm` is treated as unobstructed (O1, settled).

The 0.35 this plan previously carried as a bare constant was that
0.3536 rounded, so S8 changes the unobstructed case too: `c` rises by
1.0%, `slope` with it, and `half_width` falls by the same 1% before
`ceil`, which can move a sweep end by one step. That is accepted
deliberately — the derived value is the correct one and the rounded
one was never measured — not claimed to be a no-op. The wing-slope
check is what catches it if the 1% matters on a rig:
`get_sweep_plan` reports predicted and measured side by side.
The step is the width that fits at most `points`
samples (default 9) across the sweep, floored at half a critical focus
zone because samples closer together than that measure the same
focus; `points` is a ceiling, not a promise (O2), `get_sweep_plan`'s
`points` is the count before the focuser's bounds are applied, and a
centre near a bound walks fewer. `seeing_fwhm_arcsec` (default 2.5) stands in for the focused
HFR until the record has one. Every optics name in the block is a field
of `get_train_info.optics` (D14); `pixel_scale_arcsec_per_pixel` is
`rp`'s own derivation.
A filter with no configured wavelength, and a train with no wheel in
its path, use 550 nm; a guiding train with a wheel in its light path
(O4 rule 5) sizes from the selected filter's wavelength like any
other train, and `ctx.wavelength_of` already resolves it that way.

The measured wing slope from each run (D5) is the check on all of
this: `slope = c × microns_per_step / (N × pixel_size_um)`, so a slope
far from the predicted one means a wrong value for any input on that
line — `microns_per_step`, the `focal_length_mm` and `aperture_mm`
behind `N`, the camera's `pixel_size_um`, or, since O1 put `c` on the
obstruction, `obstruction_mm`. `get_sweep_plan` reports
the predicted and measured slopes side by side with the optical facts
they were derived from, both in pixels per 100 steps (the `wing_slope`
unit; the block's `slope` is per step and is reported ×100), so an
operator can see it. Every derived value can
be overridden per train in the provider's config (`trains.<id>.step_size`,
`half_width`), and a train whose optics are incomplete needs both set:
`focus_train` errors naming the missing fact otherwise.

### D10 — Triggers live in the session document

The provider reacts to nothing. `deep_sky.json` has a
`refocus-on-temperature` rule on `temperature_changed`, gated on
`params.focus` and `refocus_temperature_delta` (default 1.0 °C; 0
disables) against `session.last_focus_temperature`, which every focus
records from the result's `temperature_c`, and which fires only for the
imaging train's terminal focuser. Filter changes before a target apply
the stored offset through `focus_train` itself, because the tool takes
the filter. `rp` emits `temperature_changed` with `sensor` set to the
focuser id when a slow poll of the probe has moved by at least
`equipment.temperature_event_delta_c` since the last emission; reading
a probe is not actuation.

### D11 — The tools and their gates

| Tool | Where | Gate | Why |
|------|-------|------|-----|
| `temperature_changed` (event) | `rp` | — | A read |
| `get_train_info.optics`, `get_refocus_plan` | `rp`, new | Ungated | Reads of the train model |
| `focus_train` | provider | `gate: none` | Moves a focuser and a wheel; `rp` gates neither |
| `determine_filter_offsets` | provider | `gate: none` | Same |
| `calibrate_temperature`, `get_sweep_plan`, `get_focus_model`, `get_focus_runs`, `set_focus_offsets`, `reset_focus_model` | provider | `gate: none` | Reads and store writes; no device |

`rp`'s line is "moves the mount or exposes the optics"; none of these
does. The registration names the tools `rp` brackets with the focus
events (D15) — `focus_train` alone. `determine_filter_offsets` drives
the same devices and is still excluded, because the bracket's
`focus_complete` is one sweep's vertex and confirmation and a
procedure of `rounds × filters` sweeps has none; a triple with every
field null would mislead the Guide Focus Watch, which reads that
payload to decide whether the event touched the guiding train.
Bracketing it means first deciding which sweep the call reports as its
focus, and that is an amendment to make deliberately rather than a
detail of S5:

```json
{
  "name": "focus-model",
  "type": "tool_provider",
  "mcp_server_url": "https://localhost:11173/mcp",
  "auth": { "username": "observatory", "password": "secret" },
  "gate": {
    "focus_train": "none", "determine_filter_offsets": "none",
    "calibrate_temperature": "none", "get_sweep_plan": "none",
    "get_focus_model": "none", "get_focus_runs": "none",
    "set_focus_offsets": "none", "reset_focus_model": "none"
  },
  "focus_tools": { "focus_train": "train_id" },
  "requires_tools": [
    "get_train_info", "get_refocus_plan", "get_focuser_position",
    "get_focuser_temperature", "move_focuser", "get_filter", "set_filter",
    "capture", "measure_stars", "get_guiding_stats", "pause_guiding",
    "resume_guiding", "auto_focus"
  ]
}
```

### D12 — `focus_train` is the session's one entry point

Once S7 lands, every focus in `deep_sky.json` goes through
`focus_train` (S7): the per-target focus step, the frames-since-focus
rule, the HFR-degradation rule, the temperature rule, and the
guide-focus escalation, which calls it with `shared: true` on the
guiding train (D16) — except when the guiding train's plan has no
capture step, which the provider refuses as a guide-only walk: a
guide scope sharing no focuser, and equally a guide path whose only
focuser is the shared imaging one (O4 rule 4 — an unmotorised OAG, a
duo camera), where today's `af_sequence` yields the metric step
alone. In every such case the escalation calls `rp`'s metric
`auto_focus` directly, as the guide-only rule does, and S7 wires
both paths. That
is what keeps the record complete — a sweep the provider did not see
teaches it nothing. The guide-only rule keeps calling `rp`'s
PHD2-metric `auto_focus`, which O4 keeps for in-session guide focus;
only the training-run sweep moves to the provider (S10).

### D13 — The sweep

`focus_train` runs the V itself, through `rp`'s primitives, with the
semantics `rp`'s capture sweep has today:

1. Read the start position and temperature, predict and move (D4), and
   read the filter; `set_filter` when the call names another one.
2. Walk the grid in the focuser's backlash approach direction, one
   `move_focuser` per point, then `capture` on the train and
   `measure_stars` on the document, `frames_per_step` times when
   configured. A point's HFR is the frame's median HFR; its
   `star_count` gates it against `min_star_fraction` of the sweep's
   best count (`rejected: "sparse"`).
3. Fit the accepted points, weighted by star count; report
   `fit_r_squared`. S4 shipped `rp`'s weighted parabola, ported and
   proven on the rig's recorded sweeps; S9 replaces it with the
   hyperbola `a·√(1 + ((x − x₀)/b)²)` of the sample-gating plan's G2
   (the centre written `x₀` in both plans so it does not collide with
   D9's blur constant `c`), here and not in `rp`'s capture
   sweep, which S7 retires. The vertex bound carries over unchanged:
   an `x₀` outside the range of the accepted samples is
   `monotonic_curve` before the move to the vertex — the grid has
   been walked by then; it is the fitted position that is never
   visited — exactly as the parabola's vertex
   is today (`services/focus-model/src/sweep.rs`), because a one-wing
   sample set fits a hyperbola with a high R² and an extrapolated
   centre, and the R² knob is unset by default; S9's fit tests cover
   that case. `min_fit_r_squared` rejects a fit below
   it and defaults to unset: the quality is reported, never enforced,
   until a train's own numbers justify a threshold. A rejection is a
   fit failure like the other two and takes their path exactly —
   outcome `poor_fit` on the run, the measured `fit_r_squared` beside
   it and the samples retained in `curve_points` as for any fit
   failure, retried up to `max_attempts` with the centre
   held (there is nothing to shift toward: the curve has a minimum,
   it is just not V-shaped enough), and D6's put-back on the last
   attempt. S9 adds it to D3's outcome list; a thresholded failure
   must be as deterministic for a consumer as `not_enough_stars`.
4. Move to the vertex and take a confirmation frame; accept it within
   `confirmation_tolerance` of the lowest accepted sample, else fall
   back to that sample's position (`confirmed: false`).
5. On `not_enough_stars`, retry the same grid; on `monotonic_curve`,
   retry with the centre shifted by `half_width` toward the lowest
   accepted sample, clamped to the focuser's bounds; at most
   `max_attempts` (default 2). Report `attempts` and the wing slope
   (S1's definition: the steeper wing's least-squares slope, px per 100
   steps).
6. Guiding: when `get_refocus_plan` says the focuser is guide-coupled
   and `get_guiding_stats` reports active guiding, `pause_guiding`
   before the first move and `resume_guiding` after the confirmation
   frame, on the failure path too; a stats read that fails skips the
   handshake, and a failed resume is an error, as both are for
   `refocus_train` today.

Cancellation is checked between primitive calls, and a cancelled or
failed sweep ends in D6's put-back. The provider's BDD runs this
against the OmniSim focuser and camera, whose frames have no stars: the
grid walk, the gate, the retry, the put-back and the guiding handshake
are exercised end to end, and the fit, the confirmation and the sizing
are pinned by unit tests over recorded sweeps, as `rp`'s are today.

### D14 — The optical facts live in `rp`'s train model

Four facts the sizing needs are rig configuration, so they go where
the train model lives:

- `optical_trains[].aperture_mm` — the clear aperture. Omitted means
  no focal ratio and no derived sweep.
- `optical_trains[].obstruction_mm` — the central obstruction of that
  light path, the secondary or its baffle — a **diameter**, the same
  basis as `aperture_mm`, since D9 divides one by the other; an
  operator entering a radius would halve ε and size the sweep wrong.
  Omitted means unobstructed, which is what every train is assumed to
  be today (O1, S8). Validated
  at load and at `config.apply` the way `aperture_mm` already is: a
  finite number, not negative, and strictly less than `aperture_mm`
  when that is configured — ε ≥ 1 is not an annulus and would make the
  derived `c` meaningless. Rejected with a field error naming the
  train otherwise. The field bounds — finite, not negative — always
  apply; it is only the cross-field `< aperture_mm` comparison that
  has nothing to check when the train configures no aperture. Such a
  train has no focal ratio and no derived sweep at all (above), so a
  *valid* obstruction there is inert rather than an error, while
  `obstruction_mm: -1` is rejected with or without an aperture.
- `filter_wheels[].filters[]` accepts a name or `{name, wavelength_nm}`;
  a name-only entry has no wavelength. `get_train_info` keeps
  `filters` as names and adds `filter_wavelengths_nm` (name → nm or
  null), so `calibrator-flats` and the night documents see no change.
- `focusers[].microns_per_step` — the image-plane travel per step.
  Omitted means the driver's ASCOM `StepSize`, read once at connect
  like the position limits; a driver that does not implement it leaves
  the fact null.

`get_train_info` gains an `optics` block: `{focal_length_mm,
aperture_mm, obstruction_mm, obstruction_ratio, focal_ratio,
pixel_size_um, pixel_scale_arcsec_per_pixel, microns_per_step}`, each
null when unknown (`obstruction_ratio` is the derived ε the sizing
reads: `0.0` — not null — for a train with an aperture and no
obstruction, since unobstructed is a fact and not an unknown, and
`null` whenever `aperture_mm` is unknown, whatever obstruction is
configured, since the ratio is then undefined and such a train has no
derived sweep anyway); `pixel_size_um` is the
camera's x pixel size, the one the pixel-scale derivation already uses.
`get_focuser_position` reports `min_position`, `max_position` and the
`backlash` block beside the position (each null when the config sets
none), so the provider can clamp a grid, refuse an out-of-range
prediction and walk the grid in the approach direction, the way `rp`'s
own sweep does. Nothing here actuates; `StepSize` is a property read.

### D15 — The focus events stay `rp`'s

The stream page, the deep-sky BDD and the Guide Focus Watch all consume
`focus_started` / `focus_complete` / `focus_failed`, and a provider has
no event channel of its own (D2). The registration's `focus_tools` map
names the provider tools that are focus operations and the argument
that carries the train (D11). Around such a call `rp` resolves the
train's terminal camera and focuser, reads the position and
temperature (a failed read is null, not a refusal), emits
`focus_started` with them, forwards the call, and emits
`focus_complete` from the result's top-level `position`, `hfr`,
`best_position`, `best_hfr`, `confirmed`, `fit_r_squared`,
`samples_used` and `steps` — or `focus_failed` from the tool error,
the cancellation or the unreachable provider. A field the result
lacks is null. A call whose argument resolves to no train with a
focuser and a camera is forwarded without the bracket; the provider
answers with its own error. The payloads keep their shape, so the
stream page is unchanged and the watch's re-arm reads
`focus_complete.steps` as it reads `refocus_complete.steps`, and a
third-party focus provider gets the same treatment by declaring the
same map.

### D16 — Shared focusers: `rp` plans, the provider executes

The dependency ordering `refocus_train` derives — the train's shared
focusers upstream-first, each run in the train where it is terminal,
then the train's own focuser, which for the guiding train is the
guide step and comes last — is train-model knowledge and stays in `rp`
as a read (the plan is built for the addressed train; with the shared
focuser terminal in an imaging train, only the guiding train's plan
holds a guide step, after the imaging capture step it shares — O4
rule 2 names the two topologies where that does not hold):
`get_refocus_plan {train_id}` returns `{train_id, guide_coupled,
steps: [{focuser_id, run_train_id, camera_id, metric}]}` with `metric`
`capture` or `guide`. `focus_train {train_id, shared: true}` walks it
inside the one call: each capture step is a D13 sweep of that step's
focuser measured through its run train's camera, and the guiding
handshake of D13 is held across the capture steps and released before
the guide step.

After S10 a guide step appears only when the guiding train has a
focuser of its own, because the shared-focuser step already focused a
guide path that sits behind the imaging train's focuser (O4, rules 2
and 4) — where the shared focuser is terminal in an imaging train;
the topology rule 2 names, a shared focuser terminal only in the
guiding train, is not covered by that suppression and is S10's to
reject or define. That suppression does **not** exist yet: `af_sequence`
appends the terminal focuser unconditionally, so such a train still
yields a `metric: "guide"` step today, exactly as O4 records. When
one does appear it is last, after the shared focuser upstream of it
has been set, and it stays `rp`'s PHD2-metric `auto_focus` before and
after S10: the shared walk is an in-session path —
`guide_focus_escalation` starts it, addressed to the guiding train its
event names — and O4's rule 8 keeps
in-session guide focus on the metric sweep, which is also why the D13
handshake is released before that step rather than held across it.
The step is `rp`'s tool, carrying its own `focus_*` bracket and its
own put-back. S10's Alpaca capture sweep belongs to a training run
only, as O4 defines the term: `focus_train` or
`determine_filter_offsets` addressed to the guiding train itself,
never a step inside another train's walk. The result adds `steps`,
one per completed sweep, which the bracket carries onto
`focus_complete`. A failed step stops the sequence, puts back that
step's focuser, and releases the D13 guiding handshake if the walk
was holding it — a resume on the failure path, as D13 says — and
nothing more, because no step of this walk takes the guide camera: the guide step is the metric sweep, above, and the
facade capture of S10 belongs to a training run, whose put-back D6
covers. Completed steps are good positions. The provider does not call its own tools
through `rp` for the steps: a provider dialling `rp` to reach itself
would nest progress and cancellation through two proxies for no
gain, and the per-step record is the store's business (D3).

### D17 — Retirement

S7 removes `rp`'s capture-based `auto_focus`, `refocus_train`, and the
`auto_focus` block on imaging trains; a block on an imaging train is
rejected at load naming the train and `focus-model.json`. The
PHD2-metric sweep keeps the `auto_focus` name for guiding trains, with
the guiding train's block; O4 moves only the training-run sweep, so
this stays for the in-session path. The `focus_*` event
rows keep their payloads, emitted by D15's bracket; the `refocus_*`
rows go with `refocus_train`, their `steps` having moved onto
`focus_complete` (D16), and the Guide Focus Watch and the stream page
stop listening for them.
The sample-gating plan's G3, the measurement side, stays `rp`'s
because `measure_stars` is `rp`'s. `rp.md`'s derivation table answers
the filter-change question with `focus-model`'s offsets.

## Tool contracts

`focus_train {train_id, filter?, shared?}`. Result:

```json
{
  "train_id": "imaging",
  "filter": "Ha",
  "prediction": {
    "from_position": 29740, "start": 29812,
    "terms": { "last_good": 29766, "offset": 46, "temperature": 0 },
    "missing": ["temperature_coefficient"],
    "moved": true
  },
  "sweep": { "step_size": 27, "half_width": 108, "points": 9,
             "source": "derived", "predicted_slope": 3.7 },
  "position": 29771, "hfr": 1.04,
  "best_position": 29769, "best_hfr": 1.02,
  "confirmed": true, "fit_r_squared": 0.97, "samples_used": 8,
  "attempts": 1, "wing_slope": 3.9,
  "curve_points": [ { "position": 29663, "hfr": 4.1, "star_count": 240, "rejected": null } ],
  "temperature_c": 12.4,
  "recorded": { "last_good_updated": true, "runs": 12 },
  "model": "fresh"
}
```

`sweep.source` is `derived`, `configured` or `mixed` (one of the two
overridden); `steps` appears with `shared: true`; `prediction.skipped`
names why a prediction was not moved to. `model` is `fresh`, `stale:
focuser_id changed from f1 to f2`, `reset: focuser_id changed from f1
to f2` (this run started the fresh record) or `empty`. Errors: the train has no terminal focuser; `filter`
is not on the wheel; incomplete optics with no configured block, naming
the fact; the sweep's own errors — `not_enough_stars`,
`monotonic_curve` — with `attempts`, `curve_points` and `prediction`
attached and the focuser put back (D6).

`determine_filter_offsets {train_id, filters?, reference?, rounds?}`.
Result: `reference`, `rounds`, `offsets` (name → steps, the reference
at 0), `differences` (name → what its median was taken over),
`unmeasured` (`{filter, why}`), `sweeps` (one `{round, filter,
confirmed, position, hfr, error}` in the order they ran), `restored`
(where the call left the rig), `recorded` and `model`. Errors: no or
several wheels; an unknown filter name; a reference outside `filters`;
a `filters` list holding nothing but the reference; `rounds` outside
1–5; optics no filter's sweep can be sized from; a focuser outside its
travel; a device failure, a store the record cannot be read from, or a
cancellation, each ending the procedure with the rig put back; and no
filter measured at all. A sweep that fails to fit, or whose grid
cannot be walked, is that filter's loss for that round, not the
call's. A write the store refuses is reported rather than raised: on
the sweep whose run it was, and in `recorded` for the offsets
themselves, because by then the measurements exist and the answer
carries them.

`calibrate_temperature {train_id}`. Result: `coefficient_steps_per_c`,
`runs`, `span_c`, `residual_steps`, `filters` (the names the fitted
runs were taken through) and `unused` (`{why, runs}`, the recorded runs
the fit left out). Errors: too few runs or too narrow a span, naming
the threshold; runs no line fits; a stale record; a train with no
record.

`get_sweep_plan {train_id, filter?}`. Result: `step_size`,
`half_width`, `points`, `end_ratio`, `source`, `optics` (the facts
used), `cfz_steps`, `predicted_slope` and `measured_slope` (the most
recent run's `wing_slope`, or null), both in pixels per 100 steps, and
`configured` (the train's override block,
or null). It writes nothing and moves nothing. Errors: incomplete
optics with no configured block, naming the fact.

`get_focus_model {train_id}`: the model — identity, `model` and
`stale` as above, `reference_filter`, `offsets`, the coefficient with
its run count and span, `last_good` per filter, `runs_recorded` and
`last_run` — never the history. `get_focus_runs {train_id, limit?,
filter?}`: the runs newest first with their curve points, `limit`
default 20, and `total`. `set_focus_offsets {train_id, reference,
offsets}`: validates every name against the wheel, writes, returns the
model. `reset_focus_model {train_id}`: drops the runs, `last_good` and
the coefficient, keeps the reference and offsets, returns `dropped`,
`kept` and the model; a train with no record is an error naming it.

`rp`'s additions: `get_train_info.optics` and `filter_wavelengths_nm`
(D14); `get_refocus_plan` (D16).

## Configuration

```json
{
  "server": { "port": 11173, "bind_address": "0.0.0.0", "tls": null, "auth": null },
  "mcp_server_url": "https://localhost:11115/mcp",
  "service_auth": { "username": "observatory", "password": "secret" },
  "ca_cert": "/etc/rusty-photon/pki/ca.pem",
  "sweep": { "end_ratio": 4.0, "points": 9, "seeing_fwhm_arcsec": 2.5 },
  "trains": {
    "imaging": {
      "duration": "3s", "min_area": 4, "max_area": 500,
      "threshold_sigma": null, "frames_per_step": 1,
      "min_fit_points": 5, "min_star_fraction": 0.1,
      "min_fit_r_squared": null, "temperature_sensor": null,
      "confirmation_tolerance": 0.25, "max_attempts": 2,
      "step_size": null, "half_width": null
    }
  },
  "min_prediction_move": 5,
  "min_calibration_runs": 5,
  "min_calibration_span_c": 3.0,
  "runs_kept": 500,
  "store_path": null
}
```

`trains` is keyed by `rp`'s train id and holds what the sweep needs
that is not a fact of the optics: the exposure and the star-detection
parameters, the gate and confirmation knobs, and the two overrides. A
train absent from the map uses every default and must have complete
optics. **The example above is the target schema, not today's**:
`min_fit_r_squared` and `temperature_sensor` arrive with S9 and S11,
and `TrainConfig` sets `deny_unknown_fields`, so copying the block
verbatim into a running provider fails at load until those slices
land. Both are deliberately null by default and stay that way unless a
rig earns them: `min_fit_r_squared` enforces a fit-quality
floor that is only meaningful once a train's own sweeps have been
seen (S9, O2's sibling argument), and `temperature_sensor` names
another focuser whose probe stands in for a focuser that has none
(O3, S11). Required file (`ConditionPathExists`), `deny_unknown_fields`,
`--port` and `--bind-address` overrides, a `focus-model doctor` through
the same load path
([ADR-016](../decisions/016-service-config-ownership-and-doctor.md)).

`rp`'s side (D14): `optical_trains[].aperture_mm`,
`optical_trains[].obstruction_mm`, `filter_wheels[].filters[]` entries
as `{name, wavelength_nm}`, `focusers[].microns_per_step`.

## MVP

In: S3; S4 with the sweep, `focus_train`, `get_focus_model`,
`get_focus_runs`, `set_focus_offsets`, `reset_focus_model` and
`get_sweep_plan`; S5. That is enough for a
night to size every sweep from the telescope, start each filter at a
hand-entered or measured offset from a remembered focus, and refocus
on a temperature delta.

Deferred: S6's coefficient — the tool has shipped, but the number it
fits needs nights of runs to exist first — S7 until S4 has run on the
rig, and S8 to S11 with the open items they come from.

## Open items

Reviewed 2026-09-13. The items that were decided are recorded here as
decisions rather than deleted: the reasoning is what a later reader
needs, and "we considered it and declined" is not the same answer as
"nobody looked".

- **O1 — The blur constant. Settled: it is a train fact.** `c` is not
  a constant to calibrate but a consequence of the central
  obstruction, so `obstruction_mm` joins the train's optics (D14) and
  D9 derives `c` = 0.5·√((1+ε²)/2) from it. The old 0.35 was the ε = 0
  case rounded — the exact value is 0.3536 — so S8 moves an
  unobstructed train's `c` by 1.0% as well, which D9 records as a
  deliberate change rather than a no-op. The measured
  wing slope stays the check on the whole derivation —
  `get_sweep_plan` reports predicted and measured side by side — but
  it no longer has to absorb an obstruction the configuration can
  simply state. S8.
- **O2 — Seeing in the critical focus zone. Settled: keep the
  diffraction-only form.** The CFZ only floors the step size at
  `cfz_steps / 2`, and D9 takes `step_size` as the **maximum** of that
  floor and the geometric spacing that fits `points` samples across
  the sweep. A zone that comes out too small is therefore bounded by
  the spacing: where the CFZ floor was the larger term, shrinking it
  lowers the step toward the geometric spacing and raises the count
  toward `points`, and no further — never below the spacing that gives
  `points` samples, never above `points`
  (`services/focus-model/src/sizing.rs`, `derived_step`). `points` is
  an upper bound rather than the count a CFZ-free grid always reaches:
  the spacing is rounded up and the count floored, so a half width of
  10 at 9 points steps by 3 and walks 7 samples with no CFZ in play,
  and a small CFZ accounts for only part of any gap to `points`.
  `check_span`'s `MAX_GRID_POINTS` refusal guards explicit
  `step_size`/`half_width` overrides and an unusually large `points`,
  not this. A too-small CFZ costs at most the difference between a
  CFZ-floored grid and the configured `points` (9 by default), which
  is why this stays declined. The seeing-aware form
  is revisited if a large-aperture rig in poor seeing shows the floor
  is wrong — declined for now, not pending.
- **O3 — A train whose focuser has no probe. Settled: name the source
  or go without.** `trains.<id>.temperature_sensor` names another focuser
  whose probe stands in, read through `rp`'s existing
  `get_focuser_temperature`, so no new `rp` surface is needed and the
  operator is the one asserting the reading represents this train's
  thermal path. Where it is set it replaces the source for **every**
  temperature the provider reads or stores — D4's `temperature_now`
  for the prediction, the `temperature_c` recorded on each run, and
  therefore the readings `calibrate_temperature` fits — so a
  coefficient is never fitted across two sensors. The same rule has to
  hold for the session: `deep_sky.json` copies every focus result's
  `temperature_c` into `session.last_focus_temperature` and compares it
  with a `temperature_changed` from the train's own terminal focuser,
  so a stand-in on a train whose focuser *does* report a temperature
  would put the baseline on one sensor and the trigger on another. A
  focuser without a probe emits no `temperature_changed` at all
  (`services/rp/src/temperature_watch.rs`), which is what makes the
  intended case safe; S11 therefore makes the sweep itself refuse,
  naming both ids, when `temperature_sensor` is set and the train's
  own terminal focuser reports a temperature — in `focus_train` and
  in every sweep `determine_filter_offsets` runs alike, since both
  record the reading through the same body — rather than teaching the
  workflow about two sensors. What it does not
  touch is `rp`'s `temperature_changed` event, which is emitted per
  focuser and which `deep_sky.json` filters on the train's own
  terminal focuser; that is the sense in which the setting is
  prediction-only, and D4's wording is updated with it. There is no
  implicit fallback: a train that names nothing keeps reporting
  `missing: ["temperature"]`, because a
  coefficient fitted to ambient air that lags the tube is worse than
  no coefficient and its provenance would be invisible in the record.
  An ObservingConditions source waits on `rp` gaining those tools at
  all — rp.md lists the device as roster-and-connectivity only.

  Two limits belong to the decision. The setting is
  **prediction-only**: `deep_sky.json`'s refocus rule fires on
  `event.sensor == session.focuser_id`, the train's own terminal
  focuser, so a train with no probe still gets no automatic
  temperature trigger no matter what it names here — extending the
  trigger is separate work, not part of S11. And the stored record
  keeps no sensor provenance: `FocusRecord` carries `focuser_id`,
  `camera_id` and the filter set, so changing which probe stands in
  leaves an existing coefficient looking fresh while it was fitted
  against a different thermal source. S11 therefore adds the sensor id
  to the record's identity — the same mechanism as every other
  identity field, so a changed probe reports stale and predicts
  nothing, while `get_focus_runs` still returns the history it was
  fitted from. Not "invalidate the temperature terms": that is a
  second staleness semantics for one field, and the status row commits
  to the identity. It is not a config field on its own. S11.
- **O4 — The guiding train. The premise was wrong.** This plan said a
  guiding train has no filters, so no offsets. It can have them: an
  off-axis guider picking off behind the filter wheel, or a ZWO duo
  camera whose guide sensor shares the front aperture, both put the
  wheel in the guide path. Uncommon, but a rig to provide for — and
  `rp`'s train model already describes it, since a device affecting
  several cameras appears in several trains and the invalidation rule
  (a filter change on wheel W invalidates the focus offset of trains
  containing W) never distinguished purpose.

  What follows. The table below is **the normative statement** of
  these rules. D12, D16, the status table, the slice summaries,
  `focus-model.md` and `optical-trains.md` summarise parts of it where
  a reader needs the gist in place, and each of those summaries links
  back here and yields to this table where they differ — because
  restating the conditions without a single owner is what kept them
  drifting apart over this branch's review.

  | # | Rule | Holds when | Today |
  |---|------|-----------|-------|
  | 1 | The guiding train gets a sweep of its own | it has its own motorised focuser (a separate guide scope, an OAG with a motorised helical) | — |
  | 2 | The provider itself runs the guide sweep after the imaging train's | rule 1 holds (a guiding train whose only focuser is the shared one is rule 4, and gets no sweep) **and** the two trains **share** a focuser **and** the call is `focus_train {shared: true}` addressed to the **guiding** train: `af_sequence` builds the plan for the addressed train, so it is the guiding train's plan that holds the shared imaging capture step and then the guide step, while the imaging train's own plan never contains a guide step (`services/rp/src/equipment/trains.rs`) — provided the shared focuser is terminal in an imaging train. Two topologies break that: one terminal nowhere falls back to the addressed train in `af_sequence`, which for the guiding train makes it a second `metric: "guide"` step with no imaging capture before it; and one terminal *only* in the guiding train (imaging `[shared, own, cam]`, guiding `[shared, guide-cam]`) is run in the guiding train, so the **imaging** plan gets a `metric: "guide"` step before its own focuser and no camera ever measures the shared one through the better optics. Whether `rp` rejects those topologies at load or S10 defines their sequencing is S10's to decide, right after the refusal | the plan carries that order and the escalation walks it; the redundant step is not yet suppressed |
  | 3 | The caller orders the two trains, imaging first | every other case: the trains share **no** focuser (each plan is per train and cannot sequence the other), **or** the call is a direct training run — `focus_train` without `shared`, or `determine_filter_offsets`, on the guiding train — which reads the plan only for the guiding handshake, never for ordering, whether or not an upstream focuser is shared | session workflow's or the operator's job; S10 states it |
  | 4 | No provider training sweep and no facade capture; rule 8's in-session metric sweep is untouched | the guide path sits behind the imaging focuser **with no focuser of its own** | `af_sequence` still yields a redundant `metric: "guide"` step |
  | 5 | The guiding train has an offset table of its own | rule 1 **and** the wheel is in its light path | hand-entered through `set_focus_offsets`, which has no purpose guard, works today; **measuring** it with `determine_filter_offsets` waits on rule 7's transport |
  | 6 | Its focus participates in filter-change invalidation | the wheel is in its light path (upstream of an OAG pick-off, or in front of a shared aperture as on a duo camera) | shipped |
  | 7 | A training run captures over Alpaca, through the PHD2 camera facade | rule 1 **and** the facade of [device-claims-and-phd2-camera.md](device-claims-and-phd2-camera.md) Part B is configured (its C6), with the vendor driver excluded from the guide camera (its Part A), so the guide camera is an ordinary `rp` camera | not implemented, and not refused either: the provider receives `get_train_info.purpose` and drops it, so `focus_train` or `determine_filter_offsets` on a guiding train today attempts a capture sweep through whatever camera the train names, past the mount motion gate — the refusal is S10's first change. The transport is that plan's C6; the sweep against it is its C7, which is this plan's S10, blocked on the reconciliation both plans record |
  | 8 | The PHD2-metric sweep is used | in-session guide focus, always — `guide_focus_degraded` escalates into it | shipped, and permanent |

  Rules 1–4 are the ordering; 5–6 the offsets; 7–8 the transport. A
  *training run*, wherever this plan says it, is a direct call to the
  provider's own tools addressed to the guiding train — `focus_train
  {train_id: <guiding>}` **without** `shared`, or
  `determine_filter_offsets` on it — as distinct from in-session guide
  focus, which is `guide_focus_degraded`'s `auto_focus` and the
  `shared: true` walk on the guiding train that `guide_focus_escalation`
  starts (D16; or, for a guide scope sharing no focuser, the metric
  `auto_focus` it calls directly — D12), all keeping the guide step on the metric sweep
  (rule 8). The `shared` flag is the line, because the provider cannot
  see the caller's intent and the escalation always sets it.

  - **Why rules 1–4.** A guide path with **no focuser of its own** —
    an unmotorised OAG behind the drawtube, a duo camera's second
    sensor — has no independent focus position: focusing the imaging
    train focuses it, by construction, and a second sweep would move
    the same focuser away from the position just measured through the
    better camera. (An OAG *with* a motorised helical is not that
    case; it focuses independently and takes rule 1.) Where the guide
    path does focus itself and shares an upstream focuser, its sweep
    runs after the imaging train's because that shared focuser moves
    first and would otherwise invalidate it.

    One case the plan cannot order at all: a separate guide scope
    sharing *no* focuser with the imaging train. `get_refocus_plan` is
    per train, so the imaging train's plan then contains no guide
    focuser and the guide train's contains no imaging step — neither
    call can sequence the other, and `focus_train {shared: true}` on
    either one is not the mechanism. There the ordering is the
    caller's: a session workflow focuses the imaging train, then the
    guide train, in that order. S10 says so explicitly rather than
    leaving a guarantee the provider cannot make. Where a focuser *is*
    shared, the plan does carry the order:
    `rp`'s ordering already matches this — `get_refocus_plan` returns
    shared focusers upstream-first, then the addressed train's own
    terminal focuser, with a guiding-train step last — but the
    *suppression* does not exist yet, and S10 has to add it.
    `af_sequence` skips the terminal focuser in its shared-focuser
    loop and then appends it unconditionally
    (`services/rp/src/equipment/trains.rs`), so addressing a guiding
    train whose terminal focuser is the shared one yields a
    `metric: "guide"` step for that shared focuser today: a second
    sweep of the focuser the imaging train just set, measured through
    the worse camera. S10 makes the plan omit it, and until then the
    provider must not be described as getting this for free.
  - **Offsets follow the focuser, not the light path.** Where the
    guiding train has an independent focuser, its offsets are measured
    on it and not inherited: a different focuser and
    `microns_per_step` mean the imaging train's table does not
    transfer even though the glass is the same.
    `determine_filter_offsets` accepts such a train, and where the wheel
    is the imaging train's — an OAG behind it, a duo camera — it must
    not run while an imaging session is using that wheel, which
    nothing today can enforce (a separate guide scope with a wheel of
    its own has no such conflict): the provider's claim
    serialises only its own calls, `rp` leaves `set_filter` and
    `capture` open to every other client, and the mount motion gate
    covers mount motion alone. The `rp`-wide train lease C7 adds (device-claims plan D12) is
    the natural home for that guarantee; until it exists the
    requirement is an operator precondition, not a guarantee. Where the focuser is shared, there is nothing to
    measure — the offset in steps is the same number for both paths
    because it is the same focuser moving the same distance — and the
    guiding train simply has no offsets of its own.
  - **A training run captures through the guide camera, over
    Alpaca — through the PHD2 camera facade.** How `rp` reaches the
    guide camera is decided in
    [device-claims-and-phd2-camera.md](device-claims-and-phd2-camera.md),
    which landed while this section was being reviewed and settles the
    question it had been circling: PHD2 keeps the guide camera at the
    SDK level, `phd2-guider` serves it as an ASCOM Alpaca **Camera** on
    port 11128 backed by PHD2's own `capture_single_frame` +
    `save_image` (that plan's Part B, D9–D11), and `rp` lists the facade
    in `cameras[]` like any other camera, so it is a legal terminal
    camera of the guiding train and the provider's ordinary capture
    sweep — `move_focuser`, `capture`, `measure_stars` — runs against it
    (its D12). It is still Alpaca, which is what rp.md's integration
    tenet 4 (*Remote interfaces only*) requires. **This plan's S10 is
    that plan's C7**, and this section defers to it rather than
    restating its protocol; where the two differ, that plan governs the
    transport and this one the focusing.

    What that settles, and what earlier revisions of this section got
    wrong:

    - *No handover, and no lease over a vendor-driver session.* This
      section first had PHD2 releasing the camera, then PHD2 as a second
      client of the vendor Alpaca driver with `rp` leasing its own
      session. Neither is a path: PHD2 has no Alpaca backend on Linux or
      macOS, and time-slicing a vendor SDK device between a driver and
      PHD2 is the failure mode that plan's Part A exists to prevent (its
      D13). Part A's `claims` block is what keeps the vendor driver off
      the guide camera in the first place, and enabling the facade
      without that exclusion is a misconfiguration that plan names as
      one.
    - *The precondition is PHD2 `Stopped`, and `rp` does not stop it.*
      The facade permits capture from `Stopped` alone and refuses every
      other state naming it — an allowlist, because `Paused` can still
      be looping (its D11). Stopping guiding is the operator's or the
      workflow's explicit `guiding/stop`, never a side effect of a focus
      run. The snapshot-and-restore sequence this section used to
      specify is therefore gone, and with it the question of restoring
      guiding: a training run that finds PHD2 anywhere but `Stopped` is
      refused before anything actuates, which is the deterministic
      outcome D6 wants, and a run that starts stopped ends stopped.
    - *Exclusivity is three leases, all C7's.* A **sweep lease** on the
      facade, acquired before the first focuser move and released in a
      guard, with an owner token and a bounded TTL so a dead `rp` cannot
      hold PHD2 hostage; an **`rp`-wide train lease** that drains
      in-flight captures on the affected trains before the first move;
      and a **mount-motion lease across the whole sweep**, closing the
      `imaging_permit` exemption for guide-train cameras (its D12). The
      provider's one-run claim reaches none of these; the provider takes
      them through whatever `rp` surface C7 defines, before the first
      move, and treats a refusal as a refusal.
    - *The fail-safe is per frame.* Sole control of PHD2 during a sweep
      is a deployment precondition; the facade re-checks the state on
      every frame and a frame from a PHD2 that left `Stopped` aborts the
      sweep with a structured error naming the state, after which D6's
      put-back runs. That replaces the geometry read-back abort earlier
      revisions asked for.
    - *The image path.* A full frame comes from `save_image` through the
      facade's `image_dir`, never from `get_star_image`, a ≤32 px cutout
      that needs a selected star (its D13) — the #1187 donuts were 26–30
      px across. The completion watermark that keeps `save_image` from
      returning the previous frame is that plan's largest unknown and
      C6's to demonstrate against a live PHD2; nothing here depends on
      which candidate wins.

    What this plan still owns in S10: the purpose guard (rule 7's Today
    cell); accepting a guiding train in `focus_train` and
    `determine_filter_offsets` under rules 1–7; the imaging-first
    ordering of rules 2–3 and the two topologies rule 2 leaves open;
    suppressing the redundant metric step; and the record, sizing and
    put-back semantics of a guiding-train run — D9 reads the facade's
    `PixelSizeX`, which is configuration there, and D6's put-back
    releases the three leases on every failure and cancellation path.
    C7's design phase chooses between landing the guide capture sweep in
    this provider as a mode of `focus_train` — which that plan calls the
    likely right answer and this plan's D1 and D17 already assume — or a
    separately named `rp` operation. This plan records the first as its
    position and S10 as **blocked on that choice**, mirroring the
    dependency that plan records against S7 and D17: neither slice lands
    before the two agree on the one contract.

    rp.md's "the guide camera is never captured through" (§ Guide-train
    sweep) and the same sentence in optical-trains.md become conditional
    on the facade being configured; that plan's D12 lists both and C7
    makes the edit.
  - **The PHD2-metric sweep stays** for the in-session case, where
    guiding is running and the loop itself is the measurement. That is what
    `guide_focus_degraded` escalates into, and it is unaffected.
  S10.
- **O5 — Adaptive compensation between frames. Deferred, gated on
  data, not on design.** Ekos moves the focuser as the temperature
  drifts without a sweep. The shape is already settled — a
  `deep_sky.json` rule on `temperature_changed` calls a provider tool
  that returns a step delta and the workflow moves it, never the
  provider acting on the event itself (workspace tenet 3, *No
  actuation on connect*) — so what is missing
  is only confidence in the number: it waits until
  `calibrate_temperature` has fitted coefficients over several nights
  and more than one train.
- **O6 — Altitude and rotator terms. Declined.** Ekos models both. No
  rig here has shown flexure the temperature term cannot absorb, and
  carrying it open implied work nobody had planned. Reopen if a rig
  demonstrates it.
- **O7 — UI.** Reading the model from `ui-htmx` would be its first
  `tools/call`; out of scope, as it was for flats. It is a `ui-htmx`
  decision, not a focus-model one.
- **O8 — The last identity window. Closed: declined upstream.**
  [#1242](https://github.com/rusty-photon/rusty-photon/issues/1242)
  asked `rp` for a train generation a provider could make its writes
  conditional on. It was closed on 2026-09-12 as not needed: swapping
  hardware means restarting `rp` and its providers anyway. That is an
  **accepted operational residual**, and worth stating as one rather
  than as impossibility — nothing in the packaging enforces it.
  `rusty-photon-rp.service`'s `After=` on
  `rusty-photon-focus-model.service` is ordering only,
  the provider carries its own `Restart=`, and an `rp` config change
  takes effect on the next `rp` start, so a provider write can still
  straddle that boundary. The residual is judged small enough to
  carry, not closed by the lifecycle.

  What the provider does is the mitigation — every record carries the
  identity it was written at, every **prediction** read judges it
  against the train now, a stale record predicts nothing, and
  `determine_filter_offsets` re-reads the train after every sweep (D7,
  this document's tenet 6). `get_focus_runs` is deliberately outside
  that rule: it reads the stored history without a staleness gate,
  because reading back what happened on the old rig is the point of
  the tool and a rig change must not erase it.
  That identity is `focuser_id`, `camera_id` and the filter-name set,
  so it does **not** cover the optical facts: a changed
  `focal_length_mm`, `aperture_mm`, `microns_per_step` or S8's new
  `obstruction_mm` leaves a record fresh while `focus_train` sizes
  from the new optics and predicts from the old measurement. Two more
  D9 inputs hide behind unchanged names — a filter's
  `wavelength_nm` and the camera's `pixel_size_um` both feed the
  sizing while `filters` and `camera_id` stay identical — so S8 adds
  every optical input D9 reads to the identity, not just the four
  named above. Review raised the
  gateway race on both the S5 and S6 pull requests; that part needs no
  further work here.

## Slices

S3 is an `rp` PR with reads only: the three optical facts, the
`optics` block, `get_refocus_plan`, and the `focus_tools` registration
map with its event bracket, each pinned by BDD on the OmniSim devices.
S4 is the provider: the skeleton, the store, the sweep and the four
simplest tools, its BDD suite on `bdd-infra`'s tool-provider stub and
the OmniSim focuser and camera, and the packaging, doctor and
registration work that flats established. S5 adds
`determine_filter_offsets` with its rounds. S6 adds
`calibrate_temperature`. S7 rewires `deep_sky.json`, retires `rp`'s
capture sweep and `refocus_train`, and closes the "backlog" note in
rp.md's derivation table. A rig night validates S4 before S7: the
derived sweep against the hand-tuned block, a hand-entered offset, a
`focus_train` with a prediction that lands inside the sweep's band, a
`shared: true` walk on the guide-coupled rig, and a temperature delta
that fires the rule.

S8 is an `rp` change of one config field and two reported numbers,
the D9 line that reads them, and — provider-side — the record identity
growing every optical input D9 reads, without which an optical change
leaves old records fresh and predicting (O8). S9 is provider-side fitting with its
unit tests over generated curves across a spread of focal ratios,
`microns_per_step` and seeing floors, the rig's two recorded sweeps as
regression cases; the guide-metric sweep gets the same model in `rp`.
S10 starts by refusing what it has not built: a guard on
`get_train_info.purpose`, which the provider receives and drops today,
so a direct training call on a guiding train — `focus_train` without
`shared`, `determine_filter_offsets` — is refused by name until the
rest of the slice exists, while `focus_train {shared: true}` on it
keeps serving the escalation with its metric guide step (D16). The
rest of S10 is [device-claims-and-phd2-camera.md](device-claims-and-phd2-camera.md)'s
C7: the facade (its C6) as the guiding train's camera, its three
leases taken before the first move and released from any point, and
the provider's guiding-train support on top — and it is blocked until
that plan's C7 design phase and this plan's S7/D17 agree on the one
contract, which both plans record. It needs a rig meeting O4's rules 1
and 7 — a guide path with its own motorised focuser *and* the facade
configured with the vendor driver excluded from the guide camera. A
filter in the guide path (rule 5) is the extra condition for the
*offset table* half, not for focusing the guider. Its hardware unknown
is that plan's, not this one's: C6's capture completion watermark.
Note what the ordering rule does to its reach: a guide path behind the
imaging train's focuser — the duo camera, the unmotorised OAG — needs
none of this, because focusing the imaging train focuses it. S10 earns
its place only on a rig with an independently focusable guider, which
is worth confirming before it is scheduled ahead of S8, S9 or S11. S11 is a config field, the read behind it, and the record-identity
change that keeps a swapped probe from looking fresh; it does not
extend the refocus trigger, which stays keyed to a train's own
focuser.

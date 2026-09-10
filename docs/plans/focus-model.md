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
| S1 | `rp`: `auto_focus` retries with the same parameters, shifts on `monotonic_curve`, reports the wing slope, carries `curve_points` in fit-failure errors | In review | [#1202](https://github.com/rusty-photon/rusty-photon/issues/1202), [#1210](https://github.com/rusty-photon/rusty-photon/pull/1210) |
| S2 | `rp`: `temperature_changed` emitted from the focuser probes on a delta; `session-runner`: `refocus-on-temperature` rule in `deep_sky.json` | Merged | [#1203](https://github.com/rusty-photon/rusty-photon/issues/1203), [#1209](https://github.com/rusty-photon/rusty-photon/pull/1209) |
| S3 | `rp`: the optical facts on the train model (`aperture_mm`, filter wavelengths, `microns_per_step`) and `get_train_info.optics`; `get_refocus_plan`; the `focus_tools` registration declaration with the focus event bracket | Not started | |
| S4 | `focus-model`: crate, store, server, doctor, packaging, registration; the sweep; `focus_train`, `get_focus_model`, `set_focus_offsets`, `get_sweep_plan` | Not started | [#1204](https://github.com/rusty-photon/rusty-photon/issues/1204) |
| S5 | `focus-model`: `determine_filter_offsets` | Not started | [#1204](https://github.com/rusty-photon/rusty-photon/issues/1204) |
| S6 | `focus-model`: `calibrate_temperature` | Not started | [#1204](https://github.com/rusty-photon/rusty-photon/issues/1204) |
| S7 | `session-runner`: `deep_sky.json` calls `focus_train` everywhere it called `auto_focus` and `refocus_train`; `rp`: the capture-based `auto_focus` and `refocus_train` retire, the imaging train's `auto_focus` block goes with them, `rp.md`'s invalidation table stops saying "backlog" | Not started | |

S3 is `rp` work with no actuation in it and is the only thing S4
waits for. S5 and S6 build on S4. S7 waits for a rig night on S4. Each
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
  pixel-scale derivation. Nothing knows the aperture, the filters'
  wavelengths or the focuser's microns per step.
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
  wheel W invalidate?" with "focus offset of trains containing W
  (per-filter offsets: backlog)".
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
- the PHD2-metric sweep of the guiding train, until O4 resolves it.

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
  (`position`, `filter`, `temperature_c`, `hfr`, `at`, or null), and
  `runs`: the most recent 50 focus runs as `{at, filter, position,
  temperature_c, hfr, confirmed, fit_r_squared, step_size, half_width,
  wing_slope}`.
- **Stale means unknown.** A record whose `focuser_id`, `camera_id` or
  filter-name set no longer matches `get_train_info` is reported stale
  naming the field, as flats names a changed camera field. A stale
  record predicts nothing: `focus_train` sweeps from the current
  position and says so; `get_focus_model` reports it; `set_focus_offsets`
  and `determine_filter_offsets` overwrite it. Age alone is not a
  criterion — an old last good focus is still a measured position, and
  the temperature term is what corrects for the time since.
- **Writes are the tools' own.** `focus_train` appends a run and updates
  `last_good` on a confirmed result; `determine_filter_offsets` writes
  the reference, the offsets and `last_good`; `calibrate_temperature`
  writes the coefficient; `set_focus_offsets` writes the reference and
  offsets. `get_focus_model` and `get_sweep_plan` never write.

### D4 — The predicted start

`focus_train` computes the start position from the record and moves the
focuser there before the sweep:

```
start = last_good.position
      − offset(last_good.filter)
      + offset(filter)
      + coefficient × (temperature_now − last_good.temperature_c)
```

Every term the record cannot supply is omitted: no `last_good` means no
prediction and the sweep starts where the focuser is; no offset for a
filter means no filter term, reported as `prediction.missing:
["offset"]`; no coefficient or no temperature reading means no
temperature term. `temperature_now` is `get_focuser_temperature` on the
train's terminal focuser, read once at the start of the call. The move
is one `move_focuser`, so the backlash rules apply and the sweep's
samples and the predicted start are approached from the same side. A
prediction closer to the current position than half a critical focus
zone (D9) is not moved to, because the two positions are the same
focus; when the optics are unknown the threshold is
`min_prediction_move` steps (config, default 5). A prediction `rp`
rejects as out of the focuser's bounds is not moved to either; the
sweep runs from the current position and the result says why.

### D5 — What a run teaches

After the sweep the provider appends a run to the record with the
`position`, `hfr`, `confirmed`, `fit_r_squared`, `wing_slope`, the
sweep's `step_size` and `half_width`, the filter and the temperature
it read. Only a **confirmed** result updates `last_good`: a fallback
result is a measured position but not a trusted fit, and the session's
own refinement sweep is the place it gets confirmed. The wing slope is
what calibrates the blur constant of D9 for this train.

### D6 — Put things back

If the sweep fails after every attempt, the provider moves the focuser
back to the position it read at the start of the call, which is where
the rig was before the call, not the predicted start, and returns the
sweep's error with the prediction it made and the final attempt's
`curve_points`. Cancellation from `rp` — the unsafe transition, or the
caller going away — is observed between primitive calls and followed
by the same put-back. `determine_filter_offsets` restores the filter
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
2. Per round: focus the reference filter, then each other filter. Every
   sweep is a `focus_train {train_id, filter}` call made through `rp`,
   so each one is bracketed by the focus events, entered in `rp`'s
   in-flight registry and cancellable like any other. A filter's
   difference in a round is its confirmed position minus the round's
   confirmed reference position; a round in which either sweep was not
   confirmed contributes nothing for that filter.
3. A filter's offset is the median of its differences over the rounds.
   A filter with no usable round has no offset and is named in the
   result; the others are written. The reference, `last_good` (the last
   confirmed reference run) and every run go to the record.

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

### D9 — Sizing the sweep from the optics

The provider derives the sweep from the train's optical facts (D14)
and the filter's wavelength, before any frame is taken:

```
N          = focal_length_mm / aperture_mm                 focal ratio
CFZ_um     = 4.88 × λ_um × N²                              critical focus zone
cfz_steps  = CFZ_um / microns_per_step
slope      = c × microns_per_step / (N × pixel_um)         HFR growth, px per step
hfr_focus  = last_good.hfr, else 0.5 × seeing_fwhm_arcsec / pixel_scale
half_width = ceil(hfr_focus × sqrt(end_ratio² − 1) / slope)
step_size  = max(ceil(2 × half_width / (points − 1)), ceil(cfz_steps / 2))
```

The half width places the sweep ends at `end_ratio` times the focused
HFR (config, default 4.0, the middle of the SGP and Ekos 3–5× band),
using the geometric growth of a defocused star: a defocus of Δ microns
along the axis is a blur circle of Δ/N across, and the half-flux
radius of that disc is `c` times Δ/N, with `c` = 0.35 for an
unobstructed aperture (O1). The step is the width that gives `points`
samples (default 9) across the sweep, floored at half a critical focus
zone because samples closer together than that measure the same
focus. `seeing_fwhm_arcsec` (default 2.5) stands in for the focused
HFR until the record has one; `pixel_scale` is `rp`'s own derivation.
The guiding train and a filter with no wavelength use 550 nm.

The measured wing slope from each run (D5) is the check on all of
this: a slope far from the predicted one means a wrong
`microns_per_step` or aperture, and `get_sweep_plan` reports both
numbers side by side so an operator can see it. Every derived value can
be overridden per train in the provider's config (`trains[].step_size`,
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
| `calibrate_temperature`, `get_sweep_plan`, `get_focus_model`, `set_focus_offsets` | provider | `gate: none` | Reads and store writes; no device |

`rp`'s line is "moves the mount or exposes the optics"; none of these
does. The registration names the tools `rp` brackets with the focus
events (D15):

```json
{
  "name": "focus-model",
  "type": "tool_provider",
  "mcp_server_url": "https://localhost:11173/mcp",
  "auth": { "username": "observatory", "password": "secret" },
  "gate": {
    "focus_train": "none", "determine_filter_offsets": "none",
    "calibrate_temperature": "none", "get_sweep_plan": "none",
    "get_focus_model": "none", "set_focus_offsets": "none"
  },
  "focus_tools": { "focus_train": "train_id" },
  "requires_tools": [
    "get_train_info", "get_refocus_plan", "get_focuser_position",
    "get_focuser_temperature", "move_focuser", "set_filter", "capture",
    "measure_stars", "pause_guiding", "resume_guiding"
  ]
}
```

### D12 — `focus_train` is the session's one entry point

Once S4 lands, every focus in `deep_sky.json` goes through
`focus_train` (S7): the per-target focus step, the frames-since-focus
rule, the HFR-degradation rule, the temperature rule, and the
guide-focus escalation, which calls it with `shared: true` (D16). That
is what keeps the record complete — a sweep the provider did not see
teaches it nothing. The guide-only rule keeps calling `rp`'s
PHD2-metric `auto_focus` until O4 resolves the guiding train.

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
   `fit_r_squared`. The hyperbolic model of the sample-gating plan's G2
   lands here, not in `rp`.
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
   and the guider reports active guiding, `pause_guiding` before the
   first move and `resume_guiding` after the confirmation frame; a
   failed resume is an error, as it is for `refocus_train` today.

Cancellation is checked between primitive calls, and a cancelled or
failed sweep ends in D6's put-back. The provider's BDD runs this
against the OmniSim focuser and camera, whose frames have no stars: the
grid walk, the gate, the retry, the put-back and the guiding handshake
are exercised end to end, and the fit, the confirmation and the sizing
are pinned by unit tests over recorded sweeps, as `rp`'s are today.

### D14 — The optical facts live in `rp`'s train model

Three facts the sizing needs are rig configuration, so they go where
the train model lives:

- `optical_trains[].aperture_mm` — the clear aperture. Omitted means
  no focal ratio and no derived sweep.
- `filter_wheels[].filters[]` accepts a name or `{name, wavelength_nm}`;
  a name-only entry has no wavelength. `get_train_info` keeps
  `filters` as names and adds `filter_wavelengths_nm` (name → nm or
  null), so `calibrator-flats` and the night documents see no change.
- `focusers[].microns_per_step` — the image-plane travel per step.
  Omitted means the driver's ASCOM `StepSize`, read once at connect
  like the position limits; a driver that does not implement it leaves
  the fact null.

`get_train_info` gains an `optics` block: `{focal_length_mm,
aperture_mm, focal_ratio, pixel_size_um, pixel_scale_arcsec_per_pixel,
microns_per_step}`, each null when unknown. Nothing here actuates;
`StepSize` is a property read.

### D15 — The focus events stay `rp`'s

The stream page, the deep-sky BDD and the Guide Focus Watch all consume
`focus_started` / `focus_complete` / `focus_failed`, and a provider has
no event channel of its own (D2). The registration's `focus_tools` map
names the provider tools that are focus operations and the argument
that carries the train (D11). Around such a call `rp` resolves the
train's terminal camera and focuser, reads the position and
temperature, emits `focus_started` with them, forwards the call, and
emits `focus_complete` from the result's top-level `position`, `hfr`,
`best_position`, `best_hfr`, `confirmed`, `fit_r_squared` and
`samples_used` — or `focus_failed` from the tool error. A field the
result lacks is null. The payloads keep their shape, so the stream
page and the watch's re-arm are unchanged, and a third-party focus
provider gets the same treatment by declaring the same map.

### D16 — Shared focusers: `rp` plans, the provider executes

The dependency ordering `refocus_train` derives — the train's shared
focusers upstream-first, each run in the train where it is terminal,
then the train's own focuser, the guiding-train step last — is
train-model knowledge and stays in `rp` as a read:
`get_refocus_plan {train_id}` returns `{train_id, guide_coupled,
steps: [{focuser_id, run_train_id, camera_id, metric}]}` with `metric`
`capture` or `guide`. `focus_train {train_id, shared: true}` walks it:
one `focus_train` per capture step through `rp`, the guiding-train
step as `rp`'s metric `auto_focus` (O4), the guiding handshake of D13
held across the capture steps and released before the guide step. The
result adds `steps`, one per completed sweep. A failed step stops the
sequence and puts back only that step's focuser; completed steps are
good positions.

### D17 — Retirement

S7 removes `rp`'s capture-based `auto_focus`, `refocus_train`, and the
`auto_focus` block on imaging trains; a block on an imaging train is
rejected at load naming the train and `focus-model.json`. The
PHD2-metric sweep keeps the `auto_focus` name for guiding trains, with
the guiding train's block, until O4 moves it. The `focus_*` and
`refocus_*` event rows keep their payloads, emitted by D15's bracket.
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
             "source": "derived", "predicted_slope": 0.037 },
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

`sweep.source` is `derived` or `configured`; `steps` appears with
`shared: true`. `model` is `fresh`, `stale: focuser_id changed from f1
to f2` or `empty`. Errors: the train has no terminal focuser; `filter`
is not on the wheel; incomplete optics with no configured block, naming
the fact; the sweep's own errors — `not_enough_stars`,
`monotonic_curve` — with `attempts`, `curve_points` and `prediction`
attached and the focuser put back (D6).

`determine_filter_offsets {train_id, filters?, reference?, rounds?}`.
Result: `reference`, `offsets` (name → steps), `unresolved` (names),
and `rounds`: per round, per filter `{position, hfr, confirmed}`.
Errors: no or several wheels; an unknown filter name; `rounds` outside
1–5; the first sweep's error, verbatim, with everything put back.

`calibrate_temperature {train_id}`. Result: `coefficient_steps_per_c`,
`runs`, `span_c`, `residual_steps`. Errors: too few runs or too narrow a
span, naming the threshold; a stale record.

`get_sweep_plan {train_id, filter?}`. Result: `step_size`,
`half_width`, `points`, `end_ratio`, `source`, `optics` (the facts
used), `cfz_steps`, `predicted_slope`, `measured_slope` (the most
recent run's, or null), and `configured` (the train's override block,
or null). It writes nothing and moves nothing. Errors: incomplete
optics with no configured block, naming the fact.

`get_focus_model {train_id}`: the record with `model` as above.
`set_focus_offsets {train_id, reference, offsets}`: validates every
name against the wheel, writes, returns the record.

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
      "confirmation_tolerance": 0.25, "max_attempts": 2,
      "step_size": null, "half_width": null
    }
  },
  "min_prediction_move": 5,
  "min_calibration_runs": 5,
  "min_calibration_span_c": 3.0,
  "runs_kept": 50,
  "store_path": null
}
```

`trains` is keyed by `rp`'s train id and holds what the sweep needs
that is not a fact of the optics: the exposure and the star-detection
parameters, the gate and confirmation knobs, and the two overrides. A
train absent from the map uses every default and must have complete
optics. Required file (`ConditionPathExists`), `deny_unknown_fields`,
`--port` and `--bind-address` overrides, a `focus-model doctor` through
the same load path
([ADR-016](../decisions/016-service-config-ownership-and-doctor.md)).

`rp`'s side (D14): `optical_trains[].aperture_mm`,
`filter_wheels[].filters[]` entries as `{name, wavelength_nm}`,
`focusers[].microns_per_step`.

## MVP

In: S3; S4 with the sweep, `focus_train`, `get_focus_model`,
`set_focus_offsets` and `get_sweep_plan`; S5. That is enough for a
night to size every sweep from the telescope, start each filter at a
hand-entered or measured offset from a remembered focus, and refocus
on a temperature delta.

Deferred: S6 (the coefficient needs nights of runs to exist first),
S7 until S4 has run on the rig, and every open item below.

## Open items

- **O1 — The blur constant.** `c` = 0.35 is the half-flux radius of a
  uniform disc. A central obstruction pushes the flux to the rim and
  the constant toward 0.5. The measured wing slope calibrates it per
  train; whether obstruction becomes a train fact or the calibration
  absorbs it is decided after a rig night on S4.
- **O2 — Seeing in the critical focus zone.** The classical CFZ is
  diffraction-only; the seeing-aware form scales it with the seeing
  FWHM and the aperture. It would replace the D9 first line once the
  constant has been checked against a reference.
- **O3 — A train whose focuser has no probe.** The temperature term and
  the trigger both need a reading. Borrowing another focuser's probe or
  an observing-conditions sensor is a later decision; MVP omits the
  term and reports `missing: ["temperature"]`.
- **O4 — The guiding train.** No filters, so no offsets; `last_good`
  and the coefficient apply. Its sweep reads the guider's star metric
  rather than frames and stays in `rp` as the metric `auto_focus`;
  moving it into the provider needs the metric stream as an `rp`
  primitive. Not validated in MVP.
- **O5 — Adaptive compensation between frames.** Ekos moves the focuser
  as the temperature drifts without a sweep. Worth doing once
  coefficients are trusted; it is an actuation inside a session, so it
  would be a workflow rule calling a provider tool, never the provider
  acting on an event.
- **O6 — Altitude and rotator terms.** Ekos models both; no rig here has
  shown a need.
- **O7 — UI.** Reading the model from `ui-htmx` would be its first
  `tools/call`; out of scope, as it was for flats.

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

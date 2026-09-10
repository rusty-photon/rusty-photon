# Plan: `focus-model` — a tool provider that keeps each train's focus

## Goal

`auto_focus` runs one V sweep around wherever the focuser happens to be
and knows nothing about why it is there. Nothing in the system remembers
where a train last focused, how far each filter sits from the others,
or how the focus position moves with temperature, so every refocus
starts from the previous result plus whatever drifted since, and a
filter change or a cold front puts the start outside the band the
detector can measure. The night-3 coarse sweep on the Starfront rig
([#1187](https://github.com/rusty-photon/rusty-photon/issues/1187))
failed for exactly that reason: a ±1200-step grid that reached three
times beyond the measurable band, run because nobody knew how far off
the start was.

Every mainstream package solves this the same way. It does not widen
the sweep; it corrects the *start* with per-filter offsets and a
temperature model, sizes the sweep to the optics, retries a failed
sweep with the same parameters after restoring the start, and shifts
the range when the minimum is not bracketed. This plan brings that
shape to Rusty Photon in two layers:

- **`rp` keeps the sweep.** The single V — backlash-aware moves, the
  settle rule, the sparse gate, the fit, the confirmation frame, the
  restore — is a stable built-in primitive (rp.md § Plugin-Provided
  Tools), and it needs the train model, the document store and the
  guider client only `rp` has. It gains the mainstream retry and shift
  behaviour and reports the curve's wing slope
  ([#1202](https://github.com/rusty-photon/rusty-photon/issues/1202)),
  and it emits the `temperature_changed` event its events table has
  promised since the start
  ([#1203](https://github.com/rusty-photon/rusty-photon/issues/1203)).
- **A provider owns the policy.** `focus-model` is a first-party tool
  provider shaped like `calibrator-flats`: a store keyed by train,
  tools served through `rp`'s catalog, the rig driven by calling `rp`
  back. It holds each train's reference filter, per-filter offsets,
  temperature coefficient and last good focus, predicts where a sweep
  should start, runs `rp`'s sweep from there, and learns from the
  result ([#1204](https://github.com/rusty-photon/rusty-photon/issues/1204)).
  Triggers stay in the session document, where an operator started
  them (tenet 3).

The outcome: a filter change or a temperature swing moves the focuser
to a predicted position before the sweep, the sweep is sized from data
rather than guessed, a failed sweep retries the way N.I.N.A. and Ekos
do, and the numbers that make all of that work are learned once and
kept.

## Implementation Status

| Slice | Description | Status | Issue / PR |
|-------|-------------|--------|------------|
| S1 | `rp`: `auto_focus` retries with the same parameters, shifts on `monotonic_curve`, reports the wing slope, carries `curve_points` in fit-failure errors | In review | [#1202](https://github.com/rusty-photon/rusty-photon/issues/1202) · [PR #1210](https://github.com/rusty-photon/rusty-photon/pull/1210) |
| S2 | `rp`: `temperature_changed` emitted from the focuser probes on a delta; `session-runner`: `refocus-on-temperature` rule in `deep_sky.json` | In review | [#1203](https://github.com/rusty-photon/rusty-photon/issues/1203), [#1209](https://github.com/rusty-photon/rusty-photon/pull/1209) |
| S3 | `focus-model`: crate, store, server, doctor, packaging, registration; `focus_train`, `get_focus_model`, `set_focus_offsets` | Not started | [#1204](https://github.com/rusty-photon/rusty-photon/issues/1204) |
| S4 | `focus-model`: `determine_filter_offsets` | Not started | [#1204](https://github.com/rusty-photon/rusty-photon/issues/1204) |
| S5 | `focus-model`: `calibrate_temperature`, `recommend_sweep` | Not started | [#1204](https://github.com/rusty-photon/rusty-photon/issues/1204) |
| S6 | `session-runner`: `deep_sky.json` calls `focus_train` everywhere it called `auto_focus`; `rp.md`'s invalidation table stops saying "backlog" | Not started | |

S1 and S2 are independent of each other and of the provider; either
can land first. S3 needs nothing from S1 or S2 to be useful, but S5's
`recommend_sweep` needs S1's wing slope, and S2's rule switches from
`auto_focus` to `focus_train` in S6. Each slice follows
[development-workflow.md](../skills/development-workflow.md): design-doc
update first (rp.md, session-runner.md, a new
`docs/services/focus-model.md` for S3), BDD second, code third.

## Background

### What exists

- `auto_focus` (rp.md § `auto_focus` Contract): a symmetric grid of
  `half_width` each side of the current position in `step_size`
  increments, walked in the focuser's backlash approach direction,
  weighted parabola fit over the samples that pass the sparse gate, a
  confirmation frame at the vertex with fallback to the lowest accepted
  sample, and a restore to the start on `not_enough_stars` or
  `monotonic_curve`. Per-train parameters live in the train's
  `auto_focus` block (`config/optical_train.rs`). The result carries
  `temperature_c` from the terminal focuser's probe. The guiding train
  has the PHD2-metric variant of the same sweep.
- `refocus_train`: expands one trigger into the train model's
  dependency-ordered sequence when focusers are shared.
- `focusers[].backlash`: approach-direction compensation for every move
  ([#1201](https://github.com/rusty-photon/rusty-photon/pull/1201)),
  which is what makes a predicted start land where the sweep's samples
  were measured.
- `deep_sky.json` refocuses after N frames, on HFR degradation against
  the last `final_hfr`, and on the guide-focus watch's events. It has no
  temperature rule and stores no temperature. `rp.md`'s events table
  lists `temperature_changed {sensor, value}`; nothing emits it.
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

### The numbers on the rig

Night 3 on the Starfront rig, focus at 29766: HFR 1.0 px at focus, 4 px
at 100 steps, 8 at 200, 17 at 400; between 600 and 1200 steps the field
holds 0–12 stars and the samples are fragment artefacts. By the SGP and
Ekos rules the sweep for that optic is a step of 25–30 with a half
width of 100–125. The fine block in use (±400 by 100) reaches 17× the
minimum at its far end and fits only because the gate and weights carry
it; the coarse grid (±1200 by 300) never could. A predicted start
within a few tens of steps of focus is what makes a properly sized
sweep usable at all.

### The gaps this plan closes

1. No memory of focus: nothing knows a train's last good position, its
   filter offsets or its temperature behaviour.
2. No start correction: a filter change or a temperature swing is
   absorbed by the sweep, or not at all.
3. No temperature trigger, and no event to hang one on.
4. A sweep that misses errors on the first attempt and reports nothing
   an operator could size the next one from.

## Decisions

### D1 — `rp` keeps the sweep; a provider owns the policy

`auto_focus`, the guide-metric variant, `refocus_train`, the focuser
moves and the settle and backlash rules stay in `rp`. The provider
never re-implements a sweep; it calls `auto_focus`. The line is the one
rp.md draws: stable astronomy primitives are built-ins, and a provider
composes them. The provider is `focus-model`: crate
`services/focus-model`, binary and unit `rusty-photon-focus-model`,
config `focus-model.json`, store `focus-model.redb`, port 11173 (next
to `calibrator-flats` 11170, `session-runner` 11171, `polar-align`
11172), MSI feature `FocusModel`. The name is O1.

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
  temperature_c, hfr, confirmed, fit_r_squared, wing_slope}`.
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
  offsets. `get_focus_model` and `recommend_sweep` never write.

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
prediction within `min_prediction_move` steps of the current position
(config, default 5) is not moved to. A prediction `rp` rejects as out of
the focuser's bounds is not moved to either; the sweep runs from the
current position and the result says why.

### D5 — What a run teaches

After `auto_focus` returns, the provider appends a run to the record
with the result's `final_position`, `final_hfr`, `confirmed`,
`fit_r_squared`, the filter and the temperature it read. Only a
**confirmed** result updates `last_good`: a fallback result is a
measured position but not a trusted fit, and the session's own
refinement sweep is the place it gets confirmed. When the wing slope
exists (S1), it is stored with the run for `recommend_sweep`.

### D6 — Put things back

If the sweep fails, `rp` has already restored the sweep's start, which
is the predicted position, not where the rig was before the call. The
provider then moves the focuser back to the position it read at the
start of the call, best effort and logged, and returns the sweep's error
with the prediction it made. Cancellation is forwarded to `rp` and
followed by the same put-back. `determine_filter_offsets` restores the
filter that was selected before the call and leaves the focuser at that
filter's measured position from the last completed round, which is a
measured place, never a computed one.

### D7 — Determining offsets

`determine_filter_offsets {train_id, filters?, reference?, rounds?}`,
the procedure every package documents:

1. Resolve the wheel from `get_train_info`; a train without exactly one
   filter wheel is an error naming the train. `filters` defaults to the
   wheel's list, `reference` to the stored reference, else the first
   filter of the wheel; `rounds` defaults to 2, at most 5.
2. Per round: `set_filter` to the reference and run `auto_focus`; then
   for each other filter, `set_filter` and `auto_focus`. A filter's
   difference in a round is its confirmed `final_position` minus the
   round's confirmed reference position; a round in which either sweep
   was not confirmed contributes nothing for that filter.
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

### D9 — Sizing the sweep from data

`recommend_sweep {train_id}` takes the most recent run that has a wing
slope and `last_good.hfr`, and returns the `step_size` and `half_width`
that place the sweep ends at four times the focused HFR with nine
points, next to the train's current block. It writes nothing and moves
nothing. The 3–5× band is the SGP and Ekos guidance; four is the middle
of it. A train with no slope on record gets an error saying which run
would provide one.

### D10 — Triggers live in the session document

The provider reacts to nothing. `deep_sky.json` gains a
`refocus-on-temperature` rule on `temperature_changed` (S2), gated on
`params.focus` and a `refocus_temperature_delta` parameter (default
1.0 °C; 0 disables) against `session.last_focus_temperature`, which
the first focus seeds from the result's `temperature_c`. Filter changes
before a target apply the stored offset through `focus_train` itself,
because the tool takes the filter. `rp` emits `temperature_changed`
with `sensor` set to the focuser id when a slow poll of the probe has
moved by at least `temperature_event_delta_c` (config, default 0.5)
since the last emission; reading a probe is not actuation.

### D11 — The tools and their gates

| Tool | Where | Gate | Why |
|------|-------|------|-----|
| `temperature_changed` (event) | `rp`, new emission | — | A read |
| `auto_focus` retry, shift, wing slope | `rp`, changed | Ungated (unchanged) | Focuser only |
| `focus_train` | provider | `gate: none` | Moves a focuser and a wheel; `rp` gates neither |
| `determine_filter_offsets` | provider | `gate: none` | Same |
| `calibrate_temperature`, `recommend_sweep`, `get_focus_model`, `set_focus_offsets` | provider | `gate: none` | Reads and store writes; no device |

`rp`'s line is "moves the mount or exposes the optics"; none of these
does. The registration:

```json
{
  "name": "focus-model",
  "type": "tool_provider",
  "mcp_server_url": "https://localhost:11173/mcp",
  "auth": { "username": "observatory", "password": "secret" },
  "gate": {
    "focus_train": "none", "determine_filter_offsets": "none",
    "calibrate_temperature": "none", "recommend_sweep": "none",
    "get_focus_model": "none", "set_focus_offsets": "none"
  },
  "requires_tools": [
    "get_train_info", "get_focuser_position", "get_focuser_temperature",
    "move_focuser", "set_filter", "auto_focus"
  ]
}
```

### D12 — `focus_train` is the session's one entry point

Once S3 lands, every focus in `deep_sky.json` goes through
`focus_train` (S6): the per-target focus step, the frames-since-focus
rule, the HFR-degradation rule, the temperature rule. That is what
keeps the record complete — a sweep the provider did not see teaches it
nothing. The guide-focus rules keep calling `auto_focus` and
`refocus_train` directly until O2 resolves shared focusers.

## Tool contracts

`focus_train {train_id, filter?}`. Result:

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
  "sweep": { "...": "the auto_focus result, verbatim" },
  "recorded": { "last_good_updated": true, "runs": 12 },
  "model": "fresh"
}
```

`model` is `fresh`, `stale: focuser_id changed from f1 to f2` or
`empty`. Errors: the train has no terminal focuser; `filter` is not on
the wheel; the sweep's own errors, verbatim, with `prediction` attached
and the focuser put back (D6).

`determine_filter_offsets {train_id, filters?, reference?, rounds?}`.
Result: `reference`, `offsets` (name → steps), `unresolved` (names),
and `rounds`: per round, per filter `{position, hfr, confirmed}`.
Errors: no or several wheels; an unknown filter name; `rounds` outside
1–5; the first sweep's error, verbatim, with everything put back.

`calibrate_temperature {train_id}`. Result: `coefficient_steps_per_c`,
`runs`, `span_c`, `residual_steps`. Errors: too few runs or too narrow a
span, naming the threshold; a stale record.

`recommend_sweep {train_id}`. Result: `step_size`, `half_width`,
`points`, `end_ratio`, `from_run` (its `at`), and `current` (the
train's block). Errors: no run with a wing slope.

`get_focus_model {train_id}`: the record with `model` as above.
`set_focus_offsets {train_id, reference, offsets}`: validates every
name against the wheel, writes, returns the record.

## Configuration

```json
{
  "server": { "port": 11173, "bind_address": "0.0.0.0", "tls": null, "auth": null },
  "mcp_server_url": "https://localhost:11115/mcp",
  "service_auth": { "username": "observatory", "password": "secret" },
  "ca_cert": "/etc/rusty-photon/pki/ca.pem",
  "min_prediction_move": 5,
  "min_calibration_runs": 5,
  "min_calibration_span_c": 3.0,
  "runs_kept": 50,
  "store_path": null
}
```

Required file (`ConditionPathExists`), `deny_unknown_fields`, `--port`
and `--bind-address` overrides, a `focus-model doctor` through the same
load path ([ADR-016](../decisions/016-service-config-ownership-and-doctor.md)).

## MVP

In: S1, S2, S3 with `focus_train`, `get_focus_model` and
`set_focus_offsets`, and S4. That is enough for a night to start each
filter at a hand-entered or measured offset from a remembered focus and
to refocus on a temperature delta.

Deferred: S5 (the coefficient needs nights of runs to exist first, and
`recommend_sweep` needs S1's slope), S6 until S3 has run on the rig,
and every open item below.

## Open items

- **O1 — The name.** `focus-model` describes what it owns. Alternatives
  considered: `focus-keeper`, `train-focus`. Decide before S3 mints the
  crate, unit, feature and config names.
- **O2 — Shared focusers.** `refocus_train` orders sweeps across trains
  that share a focuser; `focus_train` predicts for one train. A
  predicted start per step of a `refocus_train` sequence needs the
  provider to drive the sequence itself or `rp` to accept a start per
  step. Until then the escalation rule keeps calling `refocus_train`.
- **O3 — A train whose focuser has no probe.** The temperature term and
  the trigger both need a reading. Borrowing another focuser's probe or
  an observing-conditions sensor is a later decision; MVP omits the
  term and reports `missing: ["temperature"]`.
- **O4 — The guiding train.** No filters, so no offsets; `last_good` and
  the coefficient apply. `focus_train` on the guiding train runs the
  metric sweep through `auto_focus` unchanged. Not validated in MVP.
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

S1 and S2 are `rp` and `session-runner` PRs, each independently green.
S3 is the provider skeleton with the store and the three simplest tools,
its BDD suite on `bdd-infra`'s tool-provider stub and the OmniSim
focuser, and the packaging, doctor and registration work that flats
established. S4 adds `determine_filter_offsets` with its rounds. S5 adds
the two derived tools. S6 rewires `deep_sky.json` and closes the
"backlog" note in rp.md's derivation table. A rig night validates S3
before S6: a hand-entered offset, a `focus_train` with a prediction that
lands inside the fine sweep's band, and a temperature delta that fires
the rule.

# focus-model -- Focus Tool Provider

## Overview

`focus-model` is a tool provider ([rp.md § Plugin-Provided
Tools](rp.md#plugin-provided-tools)) that owns **knowing how to focus an
optical train**: it sizes a V-curve sweep from the train's optics and
the filter's wavelength, predicts where the sweep should start from
what it remembers of the train, walks the sweep through `rp`'s
primitive tools as an MCP client, gates and fits the samples, confirms
the vertex, retries when the minimum was not bracketed, puts the
focuser back when nothing worked, and records every run. Its memory —
each train's reference filter, per-filter offsets, temperature
coefficient, last good focus per filter and run history — lives in a
[redb](https://crates.io/crates/redb) store keyed by train. A night
document focuses a train with one `focus_train` call and a `train_id`.

`rp` keeps the physics and the measurements: `move_focuser` with its
backlash compensation and settle rule, `capture` and `measure_stars`,
the train model with its optical facts, the guider handshake, and the
`focus_started` / `focus_complete` / `focus_failed` events it emits
around the provider's `focus_train` calls. The provider never moves
anything except through `rp`'s tools. The decision record is the
[focus-model plan](../plans/focus-model.md).

### Tenets

1. **Size the sweep from the telescope, not from a guess.** The critical
   focus zone of the optics at the filter's wavelength sets the step;
   the geometric growth of a defocused star sets the half width so the
   sweep ends at a known multiple of the focused HFR. A train whose
   optics are incomplete needs a configured sweep; there is no default
   grid.
2. **Start where the last sweep ended, corrected.** The predicted start
   is the most recent confirmed focus, moved by the filter offset and
   the temperature term the record can supply. Every term the record
   cannot supply is omitted and named. The prediction is an
   optimisation: focus is reached without it.
3. **Retry with the same parameters; shift, never widen.** A failed fit
   is repeated up to `max_attempts` times, the grid shifted toward the
   lowest sample after a monotonic curve, the way N.I.N.A. and Ekos
   retry. No attempt halves or doubles the step.
4. **Put things back.** A sweep that fails after every attempt, a
   cancelled sweep and an equipment error all end with the focuser
   moved back to where it was before the call — the position read at
   the start, never the predicted one. Guiding paused for the sweep is
   resumed on every exit.
5. **Every run is recorded, the failed ones most of all.** A run carries
   its outcome, its prediction, its fit and every curve point as the
   sweep measured it — every attempt's, and the ones a device error or
   a cancellation stopped the walk after. A call that fails before the
   first frame is a run too, with its samples null. Only a confirmed
   run teaches the model where focus is; a failed run is the one an
   operator reads the morning after.
6. **Stale means unknown.** A record whose focuser, camera or filter
   set no longer matches the train predicts nothing, is reported stale
   naming the field, and is replaced by the next write. Age alone is
   not a criterion.
7. **No actuation at startup.** The provider answers `tools/list` with
   no `rp` in sight and connects to `rp` lazily, per tool call. Nothing
   moves until a client asks for focus.

## Architecture

`focus-model` is both an MCP **server** (`/mcp`, registered in `rp` as
a `type: "tool_provider"` plugin) and an MCP **client** of `rp` (the
standard `rp-mcp-client`,
[ADR-017](../decisions/017-standard-mcp-client-construction.md)). A
client of `rp` — a `session-runner` document, an operator's MCP client
— calls `focus_train` on `rp`; `rp` resolves the train named by the
argument, emits `focus_started`, proxies the call here; the tool body
connects back to `rp` and drives the focuser, the wheel, the camera and
the guider through `rp`'s primitives; the result goes back through `rp`
verbatim, with progress relayed and cancellation forwarded, and `rp`
emits `focus_complete` from it (or `focus_failed` from the error).

```
  session-runner / operator        rp (equipment gateway)              focus-model (tool provider)
  ┌───────────────────┐            ┌───────────────────────────┐        ┌────────────────────────────┐
  │ tools/call        ├───────────►│ focus_started             │        │ /mcp  focus_train          │
  │  focus_train      │            │ proxy: focus_train        ├───────►│  ├─ FocusStore (redb)      │
  │  {train_id,       │◄───────────┤ progress + result         │◄───────┤  ├─ sizing, prediction     │
  │   filter?}        │            │ focus_complete / _failed  │        │  └─ McpClient ─┐           │
  └───────────────────┘            │                           │        └───────────────┼────────────┘
                                   │ get_train_info, get_refocus_plan, get_focuser_position, get_focuser_temperature,
                                   │ get_filter, set_filter, move_focuser, capture, measure_stars,
                                   │ get_guiding_stats, pause_guiding, resume_guiding, auto_focus (guide step)
                                   └───────────────────────────┘
```

There is no cycle at startup: `rp` dials the provider once to discover
its tools, and the provider does not need `rp` to answer `tools/list`.
`rp`'s packaged unit orders itself `After=` this one so a cold boot
finds the provider up before `rp` dials it.

### Port

11173 (configurable). `/mcp` and `/health` share the `server` block —
the same `server.tls` and `server.auth` guard both, so `rp`'s
registration `auth` is the same observatory credential every other
first-party client presents.

### Transport

Stateless streamable HTTP with JSON responses (`legacy_session_mode =
false`, `json_response = true`), the stack `rp` itself serves. The
`Host` allowlist is rmcp's loopback defaults plus the machine's
hostname, the explicit bind address, and every non-loopback interface
address on a wildcard bind — the same derivation `rp` uses, so `rp` can
dial the provider by hostname or LAN address, not only through
`localhost`.

## Tools

All six tools are registered ungated (`"gate": "none"`) in `rp`'s
config: `rp`'s line is "moves the mount or exposes the optics", and
none of these does. `focus_train` is declared in the registration's
`focus_tools` map, so `rp` brackets every call with the focus event
triple ([rp.md § Tool Provider
Registration](rp.md#tool-provider-registration)).

Every tool takes a `train_id`. The provider resolves the train through
`rp`'s `get_train_info` — the terminal camera, the terminal focuser,
the sole filter wheel with its names and wavelengths, and the `optics`
block — and never carries a per-train device list of its own. A train
without a terminal focuser or without a camera is a tool error naming
the train before anything moves.

### `focus_train {train_id, filter?, shared?}`

Focuses a train: predicts the start, sizes and walks the sweep,
confirms, records.

1. Resolves the train and its `optics`; reads the focuser's position
   (with its bounds) and temperature, and the wheel's current filter
   when the train has a wheel. A `filter` argument must name a wheel
   filter; on a train without a wheel it is an error. The sweep's
   filter is the argument when given, else the current one, else
   null on a filterless train.
2. Loads the train's record and judges it against the live train
   ([Store](#store)). A stale record predicts nothing.
3. Sizes the sweep ([Sweep sizing](#sweep-sizing)) for the filter's
   wavelength and computes the [predicted start](#the-predicted-start).
   Moves to the prediction when there is one worth moving to.
4. Switches the wheel when `filter` names a filter other than the
   current one.
5. Pauses guide corrections when the focuser is guide-coupled and
   guiding is active ([Guiding](#guiding)).
6. Runs the [sweep](#the-sweep): attempts, gate, fit, confirmation.
7. Resumes guiding, appends the run to the record, updates the
   filter's last good focus on a confirmed result, and answers.

One focus run at a time: a second `focus_train` while one is in
flight is refused rather than queued, because the two would move the
same focuser, measure each other's frames and put each other back.
The reads answer throughout.

With `shared: true` the call walks `rp`'s `get_refocus_plan` for the
train instead of sweeping its own focuser alone: each `capture` step is
a full sweep of that step's focuser measured through its run train's
camera and recorded on the run train's record, a `guide` step is
`rp`'s own PHD2-metric `auto_focus` on the guiding train, and one
guiding pause is held across the capture steps and released before the
guide step, which needs the loop running to measure. A `filter` is
passed to each capture step and checked against that step's run train,
so a walk started on a filterless guiding train may still name the
filter its imaging step focuses through. A plan whose only step is the
guide step is refused before anything actuates: its result has nowhere
to go without a capture step to report. A failed step stops the
sequence and puts back only that step's focuser; completed steps are
good positions. The provider's call is one `focus_*` triple — `rp`'s
bracket around the outer call — and the guide step, being `rp`'s own
tool, carries its own triple inside it; the result adds `steps`, one
per completed sweep, which the bracket carries onto `focus_complete`.
The plan is a read: `rp` decides the order, the provider executes it,
and never calls its own tools through `rp` to do so.

Result:

```jsonc
{
  "train_id": "imaging",
  "filter": "Ha",
  "prediction": {
    "from_position": 29740,
    "start": 29812,                        // null when there is no prediction
    "terms": { "last_good": 29766, "offset": 46, "temperature": 0.0 },
    "missing": ["temperature_coefficient"],  // the terms the record could not supply
    "moved": true,
    "skipped": null                        // why a prediction was not moved to
  },
  "sweep": { "step_size": 27, "half_width": 108, "points": 9,
             "source": "derived", "predicted_slope": 3.7 },
  "position": 29771, "hfr": 1.04,
  "best_position": 29769, "best_hfr": 1.02,
  "confirmed": true, "fit_r_squared": 0.97, "samples_used": 8,
  "attempts": 1, "wing_slope": 3.9,
  "confirmation": { "document_id": "…", "hfr": 1.04, "star_count": 231, "accepted": true },
  "curve_points": [ { "position": 29663, "hfr": 4.1, "star_count": 240,
                      "document_id": "…", "rejected": null } ],
  "temperature_c": 12.4,
  "guiding_paused": false,
  "recorded": { "last_good_updated": true, "runs": 12 },
  "model": "fresh"
}
```

`position` and `hfr` are where the focuser was left and what was
measured there — the vertex when the confirmation was accepted, the
lowest accepted sample otherwise (`confirmed: false`); `best_position`
and `best_hfr` keep their fitted meaning. `sweep.source` is `derived`,
`configured` (both `step_size` and `half_width` from the train's config
block) or `mixed`. `model` is `fresh`, `empty` (no record before this
run), `stale: camera_id changed from a to b` (a stale record that
predicted nothing) or `reset: camera_id changed from a to b` (this run
replaced it). `steps` appears with `shared: true`, one
`{focuser_id, run_train_id, metric, position, hfr, confirmed}` per
completed step. `rp` reads the seven event fields off the top level.

Progress: one `notifications/progress` tick per measured frame,
`progress` counting them, `total` the sweep's `points` plus the
confirmation frame, message naming the position and the measured HFR.
A retry pushes `progress` past `total`, which is what a caller sees
when a sweep is repeated.

### `get_sweep_plan {train_id, filter?}`

The sweep `focus_train` would run, without running it: `step_size`,
`half_width`, `points`, `end_ratio`, `source`, `optics` (the facts
used), `wavelength_nm`, `cfz_steps`, `hfr_focus`, `predicted_slope`
and `measured_slope` (the filter's most recent run's `wing_slope`, or
null), both in pixels per 100 steps, and `configured` (the train's
override block, or null). With no `filter` argument it previews the
filter in the path, as `focus_train` would, and reports it. Writes
nothing, moves nothing. Errors: incomplete optics with no configured
sweep, naming the missing fact; a `filter` not on the wheel.

### `get_focus_model {train_id}`

The model, judged against the live train: `model` (`fresh`, `stale:
…` or `empty`), `stale` (every changed field), the identity
(`focuser_id`, `camera_id`, `filters`), `reference_filter`, `offsets`,
`temperature_coefficient` with `coefficient_runs` and
`coefficient_span_c`, `last_good` (one entry per filter), the run
count as `runs_recorded`, and `last_run` — never the history. An
unknown train is `rp`'s own error, relayed.

### `get_focus_runs {train_id, limit?, filter?}`

The run history, newest first, each run with its curve points:
`runs`, `total` (before `limit`), `limit` default 20, at least 1.
`filter` restricts the list to one filter's runs.

### `set_focus_offsets {train_id, reference, offsets}`

Writes the reference filter and the offsets by hand: every name must
be on the wheel, `offsets` maps filter name to integer steps relative
to the reference, and the reference maps to 0 (given or not). A train
without a wheel is an error. A stale record is replaced. Returns the
model as `get_focus_model` would.

### `reset_focus_model {train_id}`

Forgets what a re-homed or re-seated focuser invalidated: the runs,
`last_good` and the coefficient go; the reference and the offsets stay,
being differences between filters. The record also takes on the train
as it stands, which is the point of the tool after a swap the record
cannot see; `adopted` names every identity field it took over, so an
operator reading the result sees that the offsets it kept were
measured on the old one. Returns `dropped`, `kept`, `adopted` and the
model. A train with no record is an error naming it.

### Errors

Tool errors (`isError: true`, one text block) name the cause:

| Condition | Message shape |
|-----------|---------------|
| `rp` cannot be reached at `mcp_server_url` | `rp at <url> is unreachable: …` |
| Unknown train | `train not found: x` (rp's) |
| Train without a focuser or a camera | `train 'x' has no terminal focuser` / `train 'x' has no camera` |
| `filter` on a train without a wheel | `train 'x' has no filter wheel; do not pass filter` |
| Unknown filter name | `filter 'Ha' is not on train 'x' (wheel 'main-fw' has: Luminance, Red, Green, Blue)` |
| Incomplete optics and no configured sweep | `train 'x' has no derived sweep: aperture_mm is unknown; set trains.x.step_size and half_width in focus-model.json or the fact in rp's config` |
| The sweep failed after every attempt | `not enough stars: …; attempts: 2; prediction: {…}; curve_points: [{…}]` |
| The put-back itself failed | the sweep's error, then `; the focuser could not be restored to 29740: …` |
| An `rp` tool failed mid-run (device error, aborted exposure) | the `rp` message, after the put-back |
| The caller cancelled | `cancelled: <reason>`, after the put-back |
| A second `focus_train` while one is running | `a focus run is already in progress; wait for it to finish or cancel it` |
| `shared: true` on a plan with no capture step | `train 'x' has no capture step to focus` |
| `reset_focus_model` on a train without a record | `train 'x' has no focus model` |
| `get_focus_runs` with `limit` 0 | `limit must be at least 1` |

A JSON-RPC error from the provider (a malformed argument object) is
relayed by `rp` as that JSON-RPC error.

### Put-back and cancellation

Every `focus_train` body wraps the sweep in the same guard: read the
position before anything moves, run the body, then — on a fit failure
after the last attempt, an equipment error or a cancellation — move
the focuser back to that position and resume guiding if it was paused.
A successful run leaves the focuser at its result and needs no
put-back. A failed put-back is named in the error text, never masking
the sweep's own error, and a store that cannot take the run is logged
rather than substituted for it. `shared: true` puts back only the
failed step's focuser. The guiding resume runs on the same
uncancellable client as the put-back, after a successful sweep as well
as a failed one, so a cancellation arriving after the last frame
cannot leave corrections paused.

A client cancellation (a stopped document, an operator cancel, the
caller's connection dropping) reaches the provider as
`notifications/cancelled` through `rp`'s proxy. The tool body watches
its request token between primitive calls: the in-flight `rp` call is
cancelled with its own `notifications/cancelled` (so `rp` aborts the
exposure or the move instead of finishing it into the void), the body
returns `cancelled: <reason>`, the run is recorded with outcome
`cancelled`, and the put-back runs on a client whose token the
cancellation cannot reach. The body runs on its own task, so the
put-back completes even if the transport has stopped waiting for the
answer.

### Guiding

When `get_refocus_plan` reports the train `guide_coupled` — a capture
step moves a focuser the guiding train shares — and `get_guiding_stats`
reports `guiding: true`, the body calls `pause_guiding` (corrections
only, `full: false`) before the first move and `resume_guiding` after
the confirmation frame, on the failure path and on cancellation too.
A stats read that fails, or reports not guiding, skips the handshake
rather than blocking the sweep; a failed resume is an error, as it is
for `rp`'s `refocus_train`, and a sweep whose run was otherwise good is
recorded before that error surfaces. The plan read is the one that
does not skip: it is the only thing that says whether the guiding
train shares this focuser, so a plan `rp` cannot answer fails the call
before anything moves. `guiding_paused` on the result says whether the
handshake ran.

## Sweep sizing

The provider derives the sweep from the train's `optics` and the
filter's wavelength before any frame is taken:

```
N          = focal_length_mm / aperture_mm                 focal ratio
λ_um       = wavelength_nm / 1000                          the filter's wavelength, 550 nm when unknown
CFZ_um     = 4.88 × λ_um × N²                              critical focus zone
cfz_steps  = CFZ_um / microns_per_step
slope      = c × microns_per_step / (N × pixel_size_um)    HFR growth, px per step, c = 0.35
hfr_focus  = last_good.hfr for the filter, else 0.5 × seeing_fwhm_arcsec / pixel_scale_arcsec_per_pixel
half_width = ceil(hfr_focus × sqrt(end_ratio² − 1) / slope)
step_size  = max(ceil(2 × half_width / (points − 1)), ceil(cfz_steps / 2))
```

The half width places the sweep ends at `sweep.end_ratio` times the
focused HFR (default 4.0, the middle of the 3–5× band the mainstream
packages size to), using the geometric growth of a defocused star: a
defocus of Δ microns along the axis is a blur circle of Δ/N across, and
the half-flux radius of that disc is `c` times Δ/N, with `c` = 0.35 for
an unobstructed aperture. The step is the width that gives
`sweep.points` samples (default 9) across the sweep, floored at half a
critical focus zone because samples closer together than that measure
the same focus. `sweep.seeing_fwhm_arcsec` (default 2.5) stands in for
the focused HFR until the record has one for the filter. Every name in
the block is a field of `get_train_info.optics`; a train's
`step_size` and `half_width` in the config override the derived values
one by one, and a train whose optics are incomplete needs both.

Worked example, the reference rig of the BDD suite: a 500 mm f/5 train
with a 5.6 µm camera and a 2.5 µm/step focuser at 550 nm has a critical
focus zone of 67.1 µm (26.84 steps), a predicted slope of 3.125 px per
100 steps and a focused HFR of 0.54 px from 2.5″ seeing at 2.31″/px:
`half_width` 68, `step_size` 17, 9 points. The measured wing slope of
each run is the check on all of this: a slope far from the predicted
one means a wrong `microns_per_step` or aperture, and `get_sweep_plan`
reports both side by side.

## The predicted start

```
start = anchor.position
      − offset(anchor.filter)
      + offset(filter)
      + coefficient × (temperature_now − anchor.temperature_c)
```

`anchor` is the most recent `last_good` entry of the record, whatever
its filter. Every term the record cannot supply is omitted and named in
`prediction.missing`: no `last_good` means no prediction, and the sweep
starts where the focuser is. When the anchor is on another filter and
either offset is missing, the target filter's own `last_good` entry is
the anchor instead, its offset terms cancelling; without one there is
no prediction, `missing: ["offset"]`. No coefficient or no temperature
reading means no temperature term, `missing:
["temperature_coefficient"]` or `["temperature"]`. The offsets are
whole steps; the temperature term is rounded to the nearest step.

The move is one `move_focuser`, so the backlash rules apply and the
sweep's samples and the predicted start are approached from the same
side. A prediction closer to the current position than half a critical
focus zone — `min_prediction_move` steps (default 5) when the optics
are unknown — is not moved to, because the two positions are the same
focus (`skipped: "within 13 steps of the current position"`); one
outside the focuser's bounds is not moved to either (`skipped:
"outside the focuser's bounds [0, 60000]"`). The sweep then runs from
the current position.

## The sweep

The V-curve with the semantics `rp`'s capture sweep has today
([rp.md § `auto_focus` Contract](rp.md#auto_focus-contract)), through
`rp`'s primitives:

1. The grid is `centre ± half_width` in `step_size` increments, clamped
   to the focuser's bounds (points outside are dropped, not coerced),
   walked in the focuser's backlash approach direction so every sample
   is reached from the side of the final move; a grid with fewer than
   `min_fit_points` positions is an error before any motion, and so is
   one spanning more than 1000 positions, counted before the bounds
   clamp anything.
2. Per point: `move_focuser`, then `capture` on the train and
   `measure_stars` on the document, `frames_per_step` times. A point's
   HFR is the median of its frames' median HFRs over the frames with
   stars, its `star_count` the median of theirs, its `document_id` the
   last frame's; a point with no stars, or a non-finite HFR, is
   recorded with `hfr: null` and enters nothing.
3. The sparse gate: a point whose `star_count` is below
   `min_star_fraction` of the sweep's largest count is
   `rejected: "sparse"`. Fewer than `min_fit_points` accepted points is
   `not_enough_stars`.
4. The fit: a parabola in HFR against position, weighted by star count,
   with `fit_r_squared` its weighted coefficient of determination.
   `monotonic_curve` when the design matrix is singular, the leading
   coefficient is not positive, or the vertex falls outside the sampled
   grid. The hyperbolic model of the
   [sample-gating plan](../plans/auto-focus-sample-gating.md) (G2)
   replaces the parabola here when it lands.
5. Confirmation: move to the vertex, one more frame; accepted when it
   has stars, passes the gate, and measures at most
   `(1 + confirmation_tolerance)` times the lowest accepted sample.
   Rejected → the focuser moves to that lowest sample's position and
   the result says `confirmed: false`.
6. Retry: a failed fit is repeated while attempts remain, up to
   `max_attempts` — the same grid after `not_enough_stars`, the centre
   moved by `half_width` toward the lowest accepted sample after
   `monotonic_curve`, clamped to the bounds. Only the last failure puts
   the focuser back. The result reports `attempts` and `wing_slope`,
   the steeper wing's least-squares slope in pixels per 100 steps,
   fitted on the attempt that produced the result; `curve_points`
   carries every attempt's samples, in the order they were measured.

Cancellation is checked between primitive calls. The provider's BDD
runs the sweep against the OmniSim focuser and camera, whose frames
have no stars: the grid walk, the gate, the retry, the put-back, the
record and the guiding handshake are exercised end to end, and the fit,
the confirmation, the sizing and the prediction are pinned by unit
tests over recorded and synthetic sweeps.

## Store

One redb file, `focus-model.redb`, with the `rp-targets` conventions
([rp-targets.md](../crates/rp-targets.md)): a `meta` table carrying
`schema_version`, serde-tolerant record values (a field this build adds
defaults when absent; a field this build does not know is ignored on
read and dropped when the record is rewritten), a refusal to open a
file written by a newer build.

- **Path.** `store_path` when set; otherwise the platform state
  directory — `/var/lib/rusty-photon/focus-model/` on Linux (the
  packaged unit's `StateDirectory=`), `%PROGRAMDATA%\rusty-photon\focus-model\`
  on Windows, `~/Library/Application Support/rusty-photon/focus-model/`
  on macOS — resolved through `rusty-photon-config`. The parent
  directory is created on open.
- **Key.** The train id. Offsets, the reference and the temperature
  model describe one optical train together and go stale together.
- **Record.** `train_id`; the identity — `focuser_id` (the terminal
  focuser), `camera_id`, `filters` (the wheel's names at write time, or
  null); `reference_filter` (or null); `offsets` (filter name → steps
  relative to the reference, the reference at 0);
  `temperature_coefficient` (steps per °C, or null) with
  `coefficient_runs` and `coefficient_span_c`; `last_good`, a list of
  `{filter, position, temperature_c, hfr, at}` with one entry per
  filter (`filter` null on a filterless train), the most recent
  confirmed result on that filter; `runs`, the most recent `runs_kept`
  runs, newest last; `updated_at` (RFC 3339, UTC).
- **A run.** `{at, filter, outcome, error, position, hfr, best_position,
  best_hfr, fit_r_squared, samples_used, attempts, wing_slope,
  temperature_c, step_size, half_width, sweep_source, prediction,
  curve_points}`. `outcome` is `confirmed`, `fallback`,
  `not_enough_stars`, `monotonic_curve`, `cancelled` or `error` (the
  text in `error`); a failed run is null where it measured nothing.
  The curve points are `{position, hfr, star_count, document_id,
  rejected}` as the sweep measured them — every attempt's, and the
  partial walk of a run a device error or a cancellation stopped; the
  frame stays in `rp`'s
  document store under its own eviction policy and the `document_id`
  is the cross-reference for as long as the FITS sits on disk.
- **Retention.** Nothing ages out by time: an old focus position is a
  measured position, and the temperature term is what corrects for the
  time since. The cap is `runs_kept` (default 500, a season, under a
  megabyte per train); the oldest runs go first.
- **Staleness.** A record is stale when `focuser_id`, `camera_id` or
  the filter-name set differs from what `get_train_info` reports now.
  The filters are a set: the same names in another wheel order are the
  same optics, and every offset and last good focus is keyed by name,
  not by position. Each changed field is named as `<field> changed
  from <recorded> to <current>`, in the order each side reported. A
  stale record predicts nothing and is replaced by the next write:
  `focus_train` writes a fresh record holding that run
  alone and reports `model: "reset: …"`; `set_focus_offsets` writes a
  fresh record holding the offsets alone. Until then `get_focus_model`
  shows the old record with the stale fields.
- **Writes.** Every write is a read-modify-write under one lock, and
  reads the record as it stands at write time rather than the copy the
  call loaded before its sweep: a run that took ten minutes must not
  overwrite what was written while it walked. `focus_train` appends a
  run and, on a confirmed result, the filter's `last_good` entry; `set_focus_offsets` writes the
  reference and offsets; `reset_focus_model` drops the runs,
  `last_good` and the coefficient. `get_focus_model`, `get_focus_runs`
  and `get_sweep_plan` never write. `determine_filter_offsets` (S5 of
  the plan) and `calibrate_temperature` (S6) write the reference,
  offsets and coefficient when they land.

## Configuration

The config file is required (`ConditionPathExists` on the packaged
unit): `mcp_server_url` has no sensible default. Run standalone,
`--config` names it; when omitted the path resolves to the platform
default (`~/.config/rusty-photon/focus-model.json` on Linux,
`%PROGRAMDATA%\rusty-photon\focus-model.json` on Windows) via
`rusty-photon-config`.

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
  "runs_kept": 500,
  "store_path": null
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `server` | object | `{ "port": 11173 }` | The shared `ServerConfig` ([ADR-016](../decisions/016-service-config-ownership-and-doctor.md)): `port`, `bind_address`, optional `tls` / `auth`. Guards `/mcp` and `/health` alike |
| `mcp_server_url` | string | required | `rp`'s MCP endpoint, dialed per tool call |
| `service_auth` | object or null | null | HTTP Basic credential presented to `rp` — the observatory credential; sent only over verified HTTPS ([ADR-017](../decisions/017-standard-mcp-client-construction.md)) |
| `ca_cert` | string or null | null | PEM CA path used to trust a TLS-enabled `rp` |
| `sweep.end_ratio` | float | 4.0 | Where the sweep ends, as a multiple of the focused HFR; greater than 1 |
| `sweep.points` | int | 9 | Samples across the sweep; at least 3 |
| `sweep.seeing_fwhm_arcsec` | float | 2.5 | The focused HFR's stand-in until a filter has a last good focus; positive |
| `trains.<id>.duration` | humantime | `"3s"` | Per-frame exposure |
| `trains.<id>.min_area` / `max_area` | int | 4 / 500 | Star detection area bounds passed to `measure_stars` |
| `trains.<id>.threshold_sigma` | float or null | null | Detection threshold; null leaves `rp`'s default |
| `trains.<id>.frames_per_step` | int | 1 | Frames measured per grid point; 1 to 20 |
| `trains.<id>.min_fit_points` | int | 5 | Accepted samples the fit needs; at least 3 |
| `trains.<id>.min_star_fraction` | float | 0.1 | The sparse gate, in `[0, 1)`; 0 disables it |
| `trains.<id>.confirmation_tolerance` | float | 0.25 | How much worse than the lowest sample the confirmation frame may measure; at least 0 |
| `trains.<id>.max_attempts` | int | 2 | Sweeps a run may make; 1 to 5 |
| `trains.<id>.step_size` / `half_width` | int or null | null | Override the derived sweep; positive |
| `min_prediction_move` | int | 5 | The smallest prediction worth moving to when the optics are unknown; at least 1 |
| `runs_kept` | int | 500 | Runs kept per train; at least 1 |
| `store_path` | string or null | null | Override for the redb file (see [Store](#store)) |

`trains` is keyed by `rp`'s train id and holds what the sweep needs
that is not a fact of the optics. A train absent from the map uses
every default and must have complete optics. Every bound above is
checked at load (parse-don't-validate): a bad value fails startup
naming the field. `--port` / `--bind-address` override `server.port` /
`server.bind_address` from the command line. Unknown keys fail the
load (`deny_unknown_fields`).

`focus-model doctor [--config <file>] [--json]` diagnoses the config
read-only through the same load path — [doctor.md § Per-service
doctors](doctor.md). Doctor `--fix` wires `service_auth` and `ca_cert`
like every other MCP client of `rp`.

### Registration in `rp`

`rp` learns about the provider from a `plugins[]` entry
([rp.md § Tool Provider Registration](rp.md#tool-provider-registration)).
The registration carries the gate opt-out, the `focus_tools`
declaration that makes `rp` bracket `focus_train` with the focus
events, and the dependency list:

```json
{
  "name": "focus-model",
  "type": "tool_provider",
  "mcp_server_url": "https://localhost:11173/mcp",
  "auth": { "username": "observatory", "password": "secret" },
  "gate": {
    "focus_train": "none", "get_sweep_plan": "none",
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

The `rp` tools the provider calls are exactly the `requires_tools`
list. The train's optical facts — `aperture_mm`, the filters'
`wavelength_nm`, the focuser's `microns_per_step` — live in `rp`'s
config ([rp.md § Train optics](rp.md#train-optics)).

## Module Structure

```
services/focus-model/src/
  main.rs            CLI entry point (clap + ServiceRunner)
  lib.rs             ServerBuilder / BoundServer; the MCP Host allowlist
  config.rs          Config (server, rp client, sweep and per-train knobs, store_path); typed bounds
  doctor.rs          `doctor` subcommand
  error.rs           FocusModelError (thiserror)
  store.rs           FocusStore (redb), FocusRecord, FocusRun, LastGood, staleness, the cap
  mcp_client.rs      McpClient: rp-mcp-client wrapper; cancellable calls; FocusRig impl
  sizing.rs          The sweep derivation from the optics (D9) and the sweep plan
  prediction.rs      The predicted start (D4)
  sweep.rs           Grid, gate, parabola fit, confirmation, retry — the V-curve
  workflow.rs        FocusRig trait; train resolution; the focus_train body, the shared walk,
                     the guard, the record update; the read and write tool bodies
  tools.rs           rmcp ServerHandler: the six #[tool]s, progress relay, cancellation
  routes.rs          Axum router: GET /health, /mcp
```

## Testing Strategy

Testing follows the conventions in `docs/skills/testing.md`.

### BDD Tests (Cucumber)

`services/focus-model/tests/features/focus_tools.feature` runs the real
three-process topology — OmniSim, focus-model, `rp` with the provider
registered and `focus_train` declared as a focus tool — and calls the
tools **through `rp`'s proxy** with the harness MCP client, exactly as
a document would. The provider is started before `rp` (it must answer
`tools/list` on its own) with `rp`'s port pinned in advance. The
simulator's frames carry no stars, so every sweep ends in
`not_enough_stars`; the scenarios assert what that path exercises: the
tools appear in the catalog ungated; `get_sweep_plan` derives the
worked example's numbers from the optics, takes the focuser's `StepSize`
when the config sets none, reports `mixed` and `configured` for
overrides, and names the missing fact; `focus_train` walks the grid,
persists a frame per point, retries once, restores the starting
position, records the run with its curve points and prediction, and is
bracketed by `focus_started` and `focus_failed`; a filter argument
moves the wheel and an unknown one is refused; a seeded record predicts
the start and a stale one is reported, predicts nothing and is reset by
the run; `set_focus_offsets` validates and writes, `reset_focus_model`
drops the history and keeps the offsets, `get_focus_runs` pages newest
first; a cancelled sweep restores the start; a guide-coupled train
pauses and resumes guiding around the sweep, on the failure path too;
a `shared: true` walk stops at the failed step.

`auth.feature` spawns only focus-model with `server.tls` and
`server.auth` and proves `/health` and `/mcp` both require the
credential — and that `tools/list` answers with no `rp` running.
`doctor.feature` is the shared doctor smoke.

### Unit Tests

- Config: defaults, the `server` block, CLI overrides, every typed
  bound named on a bad value, unknown keys rejected, the default store
  path per platform.
- Store: round trip, the cap, newest-first reads, the per-filter last
  good, reopen, a newer schema refused, staleness per field, reset.
- Sizing: the worked example, the CFZ floor on the step, the wavelength
  fallback, the override sources, the missing-fact error.
- Prediction: each term present and absent, the same-filter fallback,
  the half-CFZ and bounds skips, rounding.
- Sweep, over synthetic and recorded curves: the grid and its clamp,
  the gate, the fit and its three failure modes, the confirmation
  verdict, the retry centre, the wing slope.
- Workflow, against a `mockall` rig: the resolution errors, the filter
  switch, the guard on failure and cancellation, the guiding
  handshake and its skips, the record written for each outcome, the
  shared walk's stop-at-failure.
- Tools: the result shapes and the error text for the argument-level
  refusals.

## Future Considerations

- **`determine_filter_offsets`** (plan S5) measures the offsets by
  focusing every filter in rounds; **`calibrate_temperature`** (S6)
  fits the coefficient over the confirmed runs. Both write the record
  fields this slice already carries.
- **The hyperbolic V-curve model** (sample-gating plan G2) replaces the
  parabola in `sweep.rs`; the recorded curve points are what it is
  validated against.
- **The blur constant** (plan O1): the measured wing slopes calibrate
  `c` per train once the rig has run enough sweeps.
- **The guiding train** (plan O4): its sweep reads the guider's metric
  and stays `rp`'s `auto_focus` until the metric stream is an `rp`
  primitive; the record shape already admits a filterless train.

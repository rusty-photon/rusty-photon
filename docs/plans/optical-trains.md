# Optical Trains Plan — group devices by light path, derive the coupling

## Goal

rp knows devices; it does not know how they relate. The pairing of
camera↔focuser↔rotator↔filter-wheel lives outside the system today — in
whoever authors the tool calls: every compound tool takes explicit id pairs
(`auto_focus {camera_id, focuser_id}`), workflows thread three or four device
ids as separate parameters, the `camera_id` back-references on `focusers[]`
and `filter_wheels[]` are declared but read by nothing, rotators are
roster-membership only, and the guider is a global service block with no
camera association.

The reference rig makes the cost concrete. Its light path is:

```
Star Adventurer GTi (mount)
└─ Askar 60F ── drawtube moved by the ZWO EAF     ← moves EVERYTHING behind it
   └─ SCOPS OAG (pick-off splits the beam)
      ├─ SCOPS helical focuser ── QHY5III715C     ← guide-only differential focus
      └─ Falcon rotator ── QHY178M cool           ← rotates the main camera ONLY
```

Two behaviors the system should derive, and today cannot express:

1. **Autofocus ordering.** The EAF moves the drawtube; the OAG, rotator, and
   both cameras ride on it, so an EAF move invalidates the guide camera's
   focus — never the reverse. Main AF must run first, guide AF after.
2. **Coupling only where it is physical.** The Falcon sits *behind* the OAG
   pick-off, so rotation touches only the main camera; guide star, guide
   field, and PHD2 calibration are unaffected. In the more common layout
   (OAG behind the rotator) the same rotator invalidates all three. Which of
   these worlds a rig lives in is configuration, not something any device
   type implies.

The outcome of this plan: an `optical_trains` block in rp's `equipment`
config that models each camera's light path as an ordered device list.
Membership expresses coupling, position expresses optical order, and rp
derives autofocus pairing/ordering, refocus-trigger routing, rotation
effects, guider invalidation, and mount-motion coordination from it —
instead of being told each one per workflow. Device *usage* stays in rp per
[ADR-016](../decisions/016-service-config-ownership-and-doctor.md); doctor
is not involved beyond its existing config-shape checks.

## Implementation Status

| Phase | Description | Status | Branch / PR |
|-------|-------------|--------|-------------|
| T0 | This plan | Merged | [#579](https://github.com/rusty-photon/rusty-photon/pull/579) |
| T1 | Config schema + validation + derived coupling model in rp (`optical_trains`, `equipment.mount.guiding`, back-ref removal, `focal_length_mm` migration) | Merged | [#586](https://github.com/rusty-photon/rusty-photon/pull/586) |
| T2 | Train-aware MCP tools: `auto_focus` by train, `refocus_train` sequence expansion, first rotator verbs | Merged | [#591](https://github.com/rusty-photon/rusty-photon/pull/591) |
| T3 | Mount motion gate (dither/slew/flip vs. in-flight exposures) | Merged | [#594](https://github.com/rusty-photon/rusty-photon/pull/594) |
| T4 | Guiding integration: rotate×guide ladder, guide-AF trigger + escalation via PHD2 metrics | Merged | [#601](https://github.com/rusty-photon/rusty-photon/pull/601) |
| T5 | DSL train addressing (`deep_sky` takes one train id, not three device ids) + watch-event trigger wiring | Merged | [#617](https://github.com/rusty-photon/rusty-photon/pull/617) |
| T6 | ui-htmx `/equipment` grouped by train; membership editing | Not started | |

Order: T1 first (everything reads the derived model), then T2/T3 in either
order, T4 after T2+T3, T5/T6 after T2. Each phase follows
[development-workflow.md](../skills/development-workflow.md): design-doc
update (rp.md / phd2-guider.md / ui-htmx.md) first, BDD second, code third.

Backlog (explicitly deferred, see Decisions 4 and 9):

- `indi-alpaca-rotator`: a small Rust binary speaking the INDI XML protocol
  over stdio that proxies any Alpaca rotator, so stock PHD2 on Linux can use
  its native rotator support. Picked up when a real OAG-behind-rotator rig
  exists.
- Upstreaming an Alpaca rotator connection to PHD2 itself.
- Multi-mount (`mount_id` on trains), passive path elements (reducers with
  derived focal length), per-filter focus offsets.
- Doctor cross-check for driver-internal auto-flip: warn when rp
  orchestrates a star-adventurer-gti mount whose driver config enables
  `flip_policy.auto_flip_during_tracking` (incompatible with the T3 motion
  gate — see rp.md § Mount Motion Gate).

## Decisions (fixed — settled interactively 2026-07-18)

1. **A train is an ordered list of roster device ids, objective side first,
   terminating in a camera.** Devices that physically affect several cameras
   appear in several trains. Coupling is *derived from shared membership*,
   ordering from list position; nothing else is coupled. The reference rig
   vs. the common OAG-behind-rotator rig differ by exactly one id in one
   list (`falcon-rotator` present in the guide train or not).

   *Amended 2026-09-05 by
   [calibrator-flats-provider](calibrator-flats-provider.md) D3:* the
   membership set also admits `equipment.cover_calibrators[]` ids —
   first entry only, at most one per train, shareable across trains.
   Decision 2 is untouched: a calibrator is an active device.

2. **No passive optics in v1.** The list holds roster devices only. Passive
   elements (OAG body, reducers, flatteners) would buy derived focal lengths
   and reducer bookkeeping; they cost schema, UI, and validation surface. The
   list shape admits them later without breaking.

3. **Guiding is mount-scoped.** The guider corrects and dithers by moving the
   mount, which moves every train on it — so the top-level `guider` block
   moves to `equipment.mount.guiding` (the mount's existing home), stating
   the invariant in the schema (no mount, no guiding). The guide train
   carries `purpose: "guiding"`, which tells rp *which camera's* focus and
   rotation state the guider depends on.

4. **Rotate on a guide-coupled train is a ladder.** The sequence is always
   pause guiding → rotate → re-select star → resume. If PHD2 reports a
   connected rotator (`get_current_equipment`), stop there: PHD2 records the
   rotator angle with each calibration and adjusts the calibration for the
   current angle when guiding restarts — exact for any Δθ, no
   recalibration. Otherwise rp calls `clear_calibration` when
   |Δθ| > `equipment.mount.guiding.recalibrate_above_deg` (default 5°; below that the
   cross-axis leak, sin Δθ, sits inside guiding's noise floor). The angle
   cannot be injected through PHD2's API — PHD2 must genuinely connect to a
   rotator driver — so the Linux bridge is backlog, and on Windows the
   ASCOM 7 chooser's dynamic Alpaca clients already work (manual host:port
   entry; our services keep UDP discovery off).

5. **Dither waits for a frame boundary, generalized to a mount motion gate**
   (v1, not deferred). Dither, slews, and meridian flips take the gate
   exclusively; imaging-train captures hold it shared. A pending motion
   request blocks *new* captures (no starvation), in-flight subs complete,
   the motion runs and settles, captures resume. Guide pulses are exempt.
   Side effect: "which train owns dither" stops being a question — any
   imaging train's cadence may request one; the gate serializes.

6. **Guide-focus triggers come from PHD2's own star metrics, and escalate.**
   `GuideStep` events carry per-frame HFD/SNR/StarMass; a degrading trend
   fires a guide-only AF (terminal focuser of the guiding train). If the
   metrics do not recover, escalate to the full sequence — shared focusers
   first, then the guide train's own — covering the case where the shared
   focuser drifted but the main train's own trigger had not yet fired.
   Guide-train AF never captures through the guide camera (PHD2 may own it
   at the SDK level); it moves the focuser and reads PHD2's metric stream.

7. **Autofocus derivations.** A camera's focuser is the *last* focuser in
   its own list. Focusers shared across trains run before train-local ones,
   using the train where the shared focuser is terminal. Moving focuser F
   marks every train containing F focus-invalid; each re-runs its own
   terminal focuser's AF. This is requirement 1 above, generalized.

8. **The inert `camera_id` back-refs on `focusers[]`/`filter_wheels[]` are
   removed, and `focal_length_mm` moves from `CameraConfig` to the train.**
   Two sources of pairing truth invite drift; focal length was always a
   property of the light path, not the sensor. Sensor-scoped facts
   (`cooler_targets_c`, `gain`, `offset`) stay on the camera. Pre-1.0 hard
   cutover; `deny_unknown_fields` makes stale configs fail loudly at load.

9. **Policy now, bridge later.** v1 ships the ladder of Decision 4 plus a
   phd2-guider warning when the train model says the guide camera is
   rotator-coupled but PHD2 reports no connected rotator. The
   `indi-alpaca-rotator` bridge is a backlog item — no rig we operate needs
   it today (on the reference rig the Falcon is not in the guide train, so
   the ladder never fires).

10. **Trains attach implicitly to the singular mount.** Unassigned roster
    devices stay legal and behave exactly as today (explicit-id tools keep
    working); trains are enrichment, not a gate.

## Design

### Config schema

New block inside rp's `equipment`, plus the relocated guider block. The
mount keeps its current home at `equipment.mount`; the top-level `guider`
block moves into it as `equipment.mount.guiding`:

```jsonc
"equipment": {
  "optical_trains": [
    { "id": "main",  "purpose": "imaging",
      "focal_length_mm": 360.0,
      "devices": ["zwo-eaf", "falcon-rotator", "qhy178m"] },

    { "id": "guide", "purpose": "guiding",
      "focal_length_mm": 360.0,
      "devices": ["zwo-eaf", "scops-focuser", "qhy715c"] }
  ],
  "mount": {
    "alpaca_url": "http://localhost:11122",
    "guiding": {
      "url": "http://localhost:11130",
      "timeout": "90s",
      "settle_pixels": 0.8, "settle_time": "10s", "settle_timeout": "60s",
      "dither_pixels": 5,
      "recalibrate_above_deg": 5.0
    }
  },
  // cameras[], focusers[], rotators[] etc. unchanged, minus the removed
  // camera_id back-refs; CameraConfig loses focal_length_mm
}
```

`devices` entries are roster ids, and every entry is an *active* roster
device — `scops-focuser` here is the pa-scops-oag Focuser (the OAG's guide
helical), not the passive OAG body, which v1 does not model. rp resolves
each id's kind from the roster. `purpose` is `"imaging" | "guiding"`. The
OAG-behind-rotator variant of the same rig inserts `"falcon-rotator"` into
the guide list before `"scops-focuser"` — that single edit flips every
derived rotation behavior.

Validation at load. Per-field invariants (the `purpose` enum,
`focal_length_mm` positivity, the `recalibrate_above_deg` range) are
enforced in the field types at deserialize, per the parse-don't-validate
convention in [development-workflow.md](../skills/development-workflow.md);
the `validate_config` pass is reserved for the cross-array graph rules,
which cannot live in a single field's type:

- every id exists in the roster and is a focuser, rotator, filter wheel, or
  (terminal only) camera;
- the last entry is a camera; a camera appears in at most one train;
- shared devices appear in a consistent relative order across trains (the
  merged order relation is acyclic);
- at most one `purpose: "guiding"` train;
- `equipment.mount.guiding` requires `equipment.mount`; a guiding train
  requires `equipment.mount.guiding`.

### Derivation rules

| Question | Rule |
|---|---|
| Which focuser focuses camera C? | Last focuser in C's train list |
| AF sequence after trigger on train T | Shared focusers of T upstream-first (each run in the train where it is terminal), then T's terminal focuser |
| What does moving focuser F invalidate? | Focus of every train containing F |
| What does rotator R rotate? | Every train containing R (angle-invalid; if one is the guiding train, apply Decision 4's ladder) |
| What does a filter change on wheel W invalidate? | Focus offset of trains containing W (per-filter offsets: backlog) |
| Who is perturbed by dither/slew/flip? | Every train on the mount (motion gate, Decision 5) |
| Pixel-scale conversions | Per-train `focal_length_mm` + the camera's reported pixel size; enables main-pixel and arcsec dither amounts alongside today's guide-cam pixels |

### Reference rigs the model must handle

1. **The reference rig** (above): shared EAF forces main-then-guide AF;
   Falcon absent from the guide train decouples rotation from guiding.
2. **OAG behind the rotator**: `falcon-rotator` in both lists; rotation
   fires the ladder of Decision 4.
3. **Separate guide scope**: two trains sharing nothing; only the mount
   couples them; no AF ordering, no rotation coupling. The model degrades to
   independent trains with zero configuration beyond membership.
4. **Piggyback dual imaging rig**: two `imaging` trains, one mount; the
   motion gate keeps either train's dither from ruining the other's
   in-flight sub.
5. **Filter wheel placement**: wheel in the main train only (the normal
   case — the OAG pick-off sits in front of it) means filter changes never
   touch the guide train; a filter drawer in front of the pick-off would
   simply appear in both lists.

### Train-aware tools (T2)

- `auto_focus` accepts `train_id` as an alternative to the
  `camera_id`+`focuser_id` pair (mutually exclusive; explicit ids keep
  working). For the guiding train the sweep is PHD2-metric-based per
  Decision 6 — that sweep is T4's deliverable (it needs the rig
  verification below), so until T4 lands, guide-train addressing is
  refused with an error naming the deferral rather than ever
  capturing through the guide camera.
- New `refocus_train {train_id, reason}`: expands one trigger into the
  dependency-ordered AF runs of Decision 7, including the pause/resume
  handshake when the guiding train is involved. Emits the existing
  `focus_*` events per step (wrapped in a `refocus_*` operation
  triple). Per-step sweep parameters come from a per-train
  `optical_trains[].auto_focus` config block (steps span trains, so
  per-call parameters cannot serve them). Expansions that include an
  AF step run *in* the guiding train error until T4, like
  `auto_focus`.
- First rotator verbs (`move_rotator`, position readback), accepting
  `rotator_id` or `train_id`; rotators graduate from roster-membership-only.
- `dither` gains optional `unit: "guide_px" | "main_px" | "arcsec"` backed
  by the pixel-scale derivation.

### Mount motion gate (T3)

An rp-internal readers-writer gate per mount. Exclusive: dither, slew,
meridian flip, `center_on_target`'s slews. Shared: `capture` on any camera
belonging to an imaging train. Pending exclusive blocks new shared acquires;
queued exclusives run FIFO. Guide pulses bypass the gate. Full contract
(exemptions, the `mount_motion_pending` event, transitively bounded waits)
in rp.md § Mount Motion Gate.

Settled in the T3 design pass — the GTi's `auto_flip_during_tracking`
([#510](https://github.com/rusty-photon/rusty-photon/pull/510)) flips inside
the driver, invisible to rp's gate: **prevention over detection**. The gate
presumes rp is the sole source of non-guiding mount motion, so
driver-planned auto-flip must stay disabled on rp-orchestrated rigs — its
shipped default, and the posture the GTi design doc already states ("hosts
like rp own flip timing themselves via `SetSideOfPier`"). rp does not
subscribe to side-of-pier changes or invalidate in-flight subs; that
detection machinery waits until a rig actually needs it. A doctor
cross-check (warn when rp orchestrates a GTi mount whose driver config
enables auto-flip) is backlog, below.

### Guiding integration (T4)

Facts verified against PHD2 docs/source (see References):

- PHD2's Rotator equipment slot is read-only for PHD2 (it never commands the
  rotator); every calibration stores the rotator angle; restored/restarted
  guiding adjusts for the current angle. A "reverse angle" setting covers
  non-ASCOM rotation direction; the docs' validation recipe (calibrate,
  rotate ~40°, restart guiding, observe) is the acceptance test.
- `get_current_equipment` reports `rotator: {name, connected}` — the hook
  for the Decision 9 warning. No API method reads or injects the angle.
- `GuideStep` events carry HFD, SNR, StarMass. `set_paused(true)` pauses
  guide *output* while looping continues; `set_paused(true, "full")` stops
  looping too.

Guide-AF sweep, to verify on the rig during T4: keep guiding (or pause
output only), step the guide focuser, read HFD per `GuideStep`, V-curve fit,
treat `StarLost` as a bracket edge. If HFD does not stream in the chosen
pause mode, fall back to `get_star_image` polling. Trigger thresholds
(HFD-trend window, escalation deadline) are T4 design-pass parameters.

Settled in the T4 design pass (contracts in rp.md § Guide-train sweep,
§ Guide Focus Watch, § Rotator Tool Details; endpoints in phd2-guider.md):

- **The trigger and escalation are events, not rp-initiated actions.**
  rp's orchestration split (rp owns primitives + monitors, the
  orchestrator owns sequencing) decides it: only the orchestrator knows
  when a refocus fits between exposures, and an rp-fired full sequence
  would collide with in-flight captures on the main camera. rp ships the
  `focus_watch` monitor emitting `guide_focus_degraded` /
  `guide_focus_escalation`; the DSL trigger wiring that invokes
  `refocus_train` on them is T5's deliverable alongside train addressing.
- **The metric sweep runs under active corrections** (PHD2 only emits
  `GuideStep` while guiding; a defocusing star drifts little). Whether
  HFD streams in paused modes stays a rig-verification item, with
  `get_star_image` polling as the recorded fallback.
- **Sampling**: `frames_per_step` (default 3) fresh frames per position
  by frame-number watermark, median HFD per position; `star_lost` /
  null-HFD frames are invalid, and a position that fills with invalid
  frames is a null sample (the bracket edge). Same parabolic fit,
  errors, and recovery as the capture sweep.
- **Watch thresholds** (`equipment.mount.guiding.focus_watch`, optional
  block): `window` 10, `degrade_ratio` 1.25, `cooldown` 10m,
  `escalation_deadline` 10m; baseline re-arms on guiding-train
  `focus_complete`/`refocus_complete`.
- **Per-purpose `auto_focus` block fields**: imaging keeps the five
  capture fields; the guiding train takes `step_size`/`half_width`
  (+ `frames_per_step`, `min_fit_points`) and rejects capture-only
  fields at load — and vice versa.
- **Guider service surface**: four new endpoints — `GET /guiding/metrics`
  (50-entry per-frame HFD/SNR/StarMass + StarLost ring), `GET /equipment`,
  `POST /calibration/clear`, `POST /star/reselect` — no new PHD2 client
  code (the library verbs all exist).
- **Decision 9 warning** = `guide_rotator_unmodeled` point event (plus
  log) when `start_guiding` settles with a rotator-coupled guide camera
  and no PHD2 rotator.

### DSL and UI (T5, T6)

- Workflows pass a single `train_id` string parameter where they thread
  `camera_id`/`focuser_id`/`filter_wheel_id` today; `deep_sky.json` shrinks
  accordingly. Device-id parameters remain valid.
- `/equipment` groups roster rows under train headers with an unassigned
  pool; train membership (ordered list) is editable there and round-trips
  through `PUT /api/config`, surfacing rp's validation errors.

Settled in the T5 design pass (contracts in rp.md § Optical Trains /
tool table and session-runner.md § `deep_sky.json`):

- **Addressing lives at the tool level, not in the document.** `capture`
  and `center_on_target` take `camera_id` *or* `train_id` (the train's
  terminal camera — guaranteed by the last-entry-is-a-camera invariant);
  `set_filter` takes `filter_wheel_id` *or* `train_id` under the
  sole-filter-wheel rule (none or several in the train is an error
  naming it — `move_rotator`'s sole-rotator rule applied to wheels).
  "Device-id parameters remain valid" means the **tools** keep device
  addressing first-class; a document that resolved trains itself would
  re-own membership knowledge rp already owns.
- **`deep_sky.json` goes train-only** — a pre-1.0 hard cutover of the
  document's parameter contract: one required `train_id`; `camera_id`,
  `focuser_id`, `filter_wheel_id`, and the sweep-geometry parameters
  (`focus_exposure`, `focus_step_size`, `focus_half_width`) retire in
  favor of the train's `auto_focus` block. `focus_min_area` /
  `focus_max_area` stay: they are measurement policy for the
  HFR-degradation trigger's `measure_basic`, which requires them by
  contract. Supporting both addressing modes inside one document would
  double every tool call site behind `if` nodes ($expr cannot omit an
  argument key) — rejected as unreadable. `calibrator_flats.json` and
  `sky_flat.json` stay device-addressed (calibration procedures;
  `get_camera_info` has no train addressing, and converting them buys
  nothing until someone asks).
- **The watch events carry `train_id`** (the guiding train) so a
  document can wire the responses without a guide-train parameter:
  `guide_focus_degraded` → guide-only metric `auto_focus` on
  `event.train_id`; `guide_focus_escalation` → full `refocus_train` on
  `event.train_id`. Both trigger bodies are try/catch-logged — a failed
  recovery sweep degrades the night, ending it would be worse — and
  neither carries a `while` gate: the events only exist during active
  guiding, the metric sweep re-checks that at the tool, and a
  blackboard gate would race the acquisition commit and silently drop
  a once-per-episode firing. The `when` gate additionally requires
  `event.train_id != null` (the watch legally emits null without a
  guiding train).
- **Addressing alternatives are published in the tool schemas** as
  presence-only `oneOf` branches, and session-runner's layer-2 catalog
  validation enforces them against the combined literal + `$expr`
  argument-name set (excluding them from the literal-value check,
  which cannot see `$expr` names). Making the addressing fields
  `Option` had silently dropped them from the schemas' `required`
  lists, so a document naming no alternative validated clean and
  failed only at run time — this restores the fail-fast, and extends
  it to the T2 tools (`auto_focus`, the rotator verbs, `refocus_train`)
  that had the same gap.
- **Guiding adoption rides T5** (the old #464 remaining slice): the
  watch triggers are meaningless in an unguided document, so
  `deep_sky.json` gains `guide` (default `false` — guiding needs a
  configured guider + guiding train, so it is opt-in) and
  `dither_every` (default `0`). Handshake: stop guiding before any
  slew (target change, meridian flip), start after centering + focus
  with `retry {3, 30s}` then fail **loudly** — a guided session that
  cannot guide must not spend the night capturing trailed frames.
  Dither failures and stop failures are logged, not fatal.
  `session.guiding` tracks the loop in the blackboard; recovery clears
  it and a stale flag costs one idempotent `stop_guiding`.

### Migration

Hard cutover (pre-1.0): configs carrying `focusers[].camera_id`,
`filter_wheels[].camera_id`, `cameras[].focal_length_mm`, or a top-level
`guider` block fail at load with named fields. The reference rig needs a
one-time hand edit alongside its next deploy (same shape as the D3
dashboard edit).

## MVP scope

**In:** T1–T6 as phased above — schema, validation, derivations, train
tools, motion gate, guiding ladder + guide-AF escalation, DSL params, UI
grouping.

**Deferred:** the Linux PHD2 rotator bridge, PHD2 upstream patch,
multi-mount, passive path elements, per-filter focus offsets,
driver-initiated flip coordination (T3's design pass settled it as
prevention — keep driver auto-flip disabled on rp-orchestrated rigs — with
a doctor cross-check as backlog).

## References

- [PHD2 manual — rotator support & calibration adjustment](https://openphdguiding.org/man/Basic_use.htm)
- [PHD2 event server API (get_current_equipment, GuideStep, set_paused)](https://github.com/OpenPHDGuiding/phd2/wiki/EventMonitoring)
- [INDI Alpaca client driver (stalled WIP)](https://indilib.org/forum/development/12750-indi-alpaca-driver-wip.html)
- [ADR-016 — config ownership: usage stays in rp](../decisions/016-service-config-ownership-and-doctor.md)
- [rp design doc](../services/rp.md), [phd2-guider design doc](../services/phd2-guider.md), [ui-htmx design doc](../services/ui-htmx.md)

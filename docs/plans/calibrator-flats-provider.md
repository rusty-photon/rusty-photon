# Plan: `calibrator-flats` as a tool provider that remembers flat timing per train

## Goal

Turn `calibrator-flats` into the first real tool provider
([mcp-sessionless](mcp-sessionless.md) D13, slice 8) and give it the one
thing neither `rp` nor a `session-runner` document can hold: a record of
the exposure time and panel brightness that produce a 50 % flat for each
optical train and filter, learned once and reused every dusk.

At the end of this plan:

- A cover calibrator is a member of an optical train, first in the list,
  and every calibrator tool in `rp` can be addressed by `train_id`.
- `calibrator-flats` serves three tools through `rp`'s catalog —
  `train_flats`, `take_flats`, `get_flat_training` — backed by a redb
  store keyed by train and filter. A night document takes flats with one
  `take_flats` call and a `train_id`.
- `calibrator-flats` has no `/runs`, no `/status`, and no flat plan in
  its config. MCP is the only way in; `/health` stays for systemd and
  `doctor`.
- The `session-runner` document port `calibrator_flats.json` is retired,
  and the record (D13, `calibrator-flats.md`) says why the provider role
  won.

## Background

### What exists

- `calibrator-flats` is a self-starting orchestrator (mcp-sessionless
  D9, slice 6/7): `POST /runs` starts a run from a config-file flat plan
  (`camera_id`, `filter_wheel_id`, `calibrator_id`, `filters[]`), the run
  drives nine `rp` tools, and `GET /status` reports the outcome. Every
  run re-derives the exposure time per filter from `initial_duration`
  with a proportional search and a brightness ladder; nothing is kept
  between runs. [`calibrator-flats.md`](../services/calibrator-flats.md).
- The same algorithm ships as the `session-runner` document
  `services/session-runner/workflows/calibrator_flats.json`, with the
  service's BDD suite as its oracle. D13 kept both on purpose as the
  worked example for choosing between a Rust orchestrator and a
  document, and declined the provider role because a proxied
  `take_flats` "would be a third implementation of the same procedure".
- `rp` aggregates tool providers since 2026-09-03 (slice 8): a
  `type: "tool_provider"` registration is dialed at startup, its tools
  are merged into the catalog under the no-shadowing rule and proxied
  with progress relay and cancellation forwarding; the registration's
  `"gate"` map opts tools out of the gated default; `requires_tools` is
  checked against the merged catalog. rp.md § Plugin-Provided Tools.
- Optical trains ([optical-trains](optical-trains.md), decisions fixed
  2026-07-18) admit cameras, focusers, rotators and filter wheels.
  `capture`, `set_filter`, `move_rotator` and `auto_focus` take "exactly
  one of device id or `train_id`". The five calibrator tools take a
  bare `calibrator_id`. rp.md § Optical Trains.

### The gaps this plan closes

- Nothing remembers flat timing. A trained value per train and filter is
  durable state — not something a document can carry across nights, and
  not calibration knowledge `rp` should own. That is what earns the
  provider role, and it is a stronger argument than the one D13 rejected.
- A provider addressed by `train_id` cannot learn what a train contains:
  `rp` has no tool that lists a train's members or a wheel's filter
  names, and `get_camera_info` reports neither gain nor offset although
  a different gain produces a wildly different median at the same timing
  and panel level.
- The calibrator is the one active device on the light path that the
  train model does not know about.

## Decisions

Settled interactively 2026-09-04.

### D1 — `calibrator-flats` becomes a tool provider; D13 is reversed for it

The provider is justified by the data it owns, not by re-implementing
the procedure. `calibrator-flats` keeps the algorithm it has and gains a
store, an MCP server, and three tools. D13's "not a tool provider" and
`calibrator-flats.md`'s "kept on purpose" paragraph are rewritten to say
so (slice 3).

### D2 — MCP only

`POST /runs` and `GET /status` are removed. A run's outcome is the tool
result; its progress is the relayed `notifications/progress`. `/health`
stays. No `ui-htmx` code calls the service today, so nothing is
orphaned. The document port goes with the routes (D1): once the provider
exists, a hand-written document that re-derives exposure every night
*is* the third implementation D13 warned about. The DSL's worked example
is a future concern (O3).

### D3 — A cover calibrator is a train member

Amends optical-trains decision 1's membership set: `devices` may also
name an `equipment.cover_calibrators[]` id. A calibrator is an active
roster device (a motor and a lamp), so decision 2 ("no passive optics")
is untouched. Rules, enforced in the shared `validate_config` pass with
dotted-path errors:

- **First entry only.** The cover sits at the objective; a calibrator
  anywhere but position 0 is an error naming the position.
- **At most one per train.** A rig with a motorized dust cap *and* a
  separate light panel (two ASCOM CoverCalibrator devices, one
  cover-only, one calibrator-only) is not modeled; the error says so.
  Capability-resolved pairs are O1.
- **Sharing allowed.** One flip-flat over the OTA covers the main camera
  and an OAG guide camera, so the same id may be first in several
  trains. The existing merged-order rule holds because it is first
  everywhere.

### D4 — Train addressing on the calibrator tools, plus the reads a provider needs

- `get_cover_state`, `open_cover`, `close_cover`, `calibrator_on`,
  `calibrator_off` take **exactly one of `calibrator_id` or `train_id`**
  — the `set_filter` shape, kept rather than going train-only so the
  trains section's promise ("devices left out of every train stay
  legal") holds and the catalog stays uniform. A train without a
  calibrator is an error naming the train. Every result carries the
  resolved `calibrator_id` and a `trains` list naming every train that
  contains it — a closed cover blinds the guide camera and a lit panel
  floods it, so the sibling trains are worth knowing (the `moved_trains`
  precedent).
- **Gating is unchanged by addressing.** `open_cover` stays gated (it
  exposes the optics); the other four stay ungated (reads, protective,
  indoor).
- **`get_train_info {train_id}`**, new, ungated: the resolved members —
  the terminal camera id, the sole filter wheel with its filter names in
  position order (or null), the calibrator (or null), focusers and
  rotator ids, `purpose`, `focal_length_mm`. `rp` stays the only owner
  of the train model; the provider never carries a per-train filter
  list.
- **`get_camera_info` reports `gain` and `offset`**, read from the
  device (null when unsupported), so a record pins what the sensor
  actually ran at. It stays camera-addressed; the provider reaches it
  through the camera id `get_train_info` returns.

### D5 — The store

- One redb file, `calibrator-flats.redb`, in the platform data directory
  resolved by `rusty-photon-config`, overridable with `store_path`.
  Same conventions as `rp-targets` (`schema_version` key, serde-tolerant
  records; [`rp-targets.md`](../crates/rp-targets.md)).
- **Key:** train id + filter name. A filterless train stores under the
  train id with no filter — no made-up label, since the label used to
  come from the plan and there is no plan any more.
- **Record:** `duration`, `brightness` (the ladder level the search
  settled on — `take_flats` must relight the panel there), `median_adu`,
  `max_adu`, `bin_x`, `bin_y`, `gain`, `offset`, `camera_id`,
  `trained_at`. The target fraction is a fixed 50 % — not a config
  field, not stored.
- **A converged search overwrites** the record for the same train and
  filter. **An unconverged search writes nothing** and leaves the old
  record in place.
- **Stale means untrained.** A record whose `camera_id`, `max_adu`,
  binning, `gain` or `offset` no longer match what `rp` reports is
  refused as untrained, naming the filter and the field that changed.
  Age alone is not a criterion.
- **No write-back, no aging model.** `take_flats` never modifies the
  store; panel aging, a dying lamp or a powered-off panel all surface
  as the D7 warning and the operator decides whether to retrain (O5).

### D6 — The tools and their gates

`rp`'s line (mcp-sessionless D5) is "moves the mount or exposes the
optics". Provider tools are gated by default (slice 8) unless the
registration opts them out; the opt-out lives in `rp`'s config (D10).

| Tool | Where | Gate | Why |
|------|-------|------|-----|
| `get_train_info` | `rp`, new | Ungated | A read |
| `get_cover_state`, `close_cover`, `calibrator_on`, `calibrator_off` | `rp`, gain `train_id` | Ungated (unchanged) | Read, protective, indoor |
| `open_cover` | `rp`, gains `train_id` | **Gated** (unchanged) | Exposes the optics |
| `get_flat_training` | provider | `gate: none` | A read |
| `train_flats` | provider | `gate: none` | See below |
| `take_flats` | provider | `gate: none` | See below |

`train_flats` and `take_flats` are ungated because flats are exactly
what an unsafe hour is for: the cover is closed and the roof can be too.
The optics only get exposed through `open_cover`, which `rp` gates on
its own, so the safety property holds by composition. If the roof has
to close while flats are running, that is the same as during lights or
darks: `rp`'s unsafe reaction aborts the in-flight exposure, the
provider's `capture` call fails, the provider turns the panel off,
leaves the cover closed and returns an error. Nothing in the provider
reads safety state.

Tool contracts:

- **`train_flats {train_id, filters?, brightness?}`.** `filters`
  defaults to every name the wheel reports (must be absent on a
  filterless train); `brightness` is the ladder's starting level,
  default the device maximum. Reads the cover state, closes the cover,
  then per filter: set the filter, run the proportional search with the
  brightness ladder from `initial_duration`, and write a record when it
  converges. Restores the cover only if it started open. Result:
  `trained` (one record per converged filter), `unconverged`
  (`{filter, best_duration, median_adu}` per failure), `cover_restored`,
  `warnings`. **Partial success is a normal result**, not a tool error:
  the caller reads `unconverged`. Progress: one tick per search
  iteration, message naming the filter and the measured median.
- **`take_flats {train_id, count, filters?}`.** Checks the store
  **before touching anything** and fails naming every untrained or stale
  filter, so a bad request actuates nothing. Then per filter: set the
  filter, `calibrator_on` at the stored brightness, capture `count`
  frames as `frame_type: "Flat"` at the stored duration. Each frame is
  verified per D7 without holding up the next exposure. Result: per
  filter `{filter, duration, frames, out_of_range: [{image_path,
  median_adu}]}`, `total_frames`, `cover_restored`, `warnings`.
  Progress: one tick per frame, total = `count` × filters.
- **`get_flat_training {train_id, filter?}`.** The records for the
  train, or one filter. Reports staleness against the live camera the
  same way `take_flats` decides it, so an operator can see *why* a
  filter counts as untrained.

### D7 — Flat verification is asynchronous and advisory

No verification exposure. Every captured flat is measured with
`compute_image_stats` after the fact; a median outside 50 % ±
`flat_warn_tolerance` (default 10 %) gets a `warn!` naming the file and
lands in the result's `out_of_range`. The run continues. There are too
many causes (panel broken, lamp power off, panel aged, wrong filter in
the drawer) to tie a mechanism to; the operator reads the warning. The
search's own convergence `tolerance` (5 %) is a separate knob.

### D8 — A floor on flat exposure

New config `min_exposure` (humantime, default `250ms`). The search
floor is the larger of `min_exposure` and the camera's `exposure_min`. A
search that wants less than the floor counts as over-bright and steps
the brightness ladder down — the existing ladder rule applied to a
higher floor — so short flats where shutter timing dominates are
avoided by dimming the panel, not by accepting them.

### D9 — Cancellation

A client cancellation (a stopped document, an operator cancel) reaches
the provider as `notifications/cancelled` through `rp`'s proxy. The tool
body watches its request token, cancels its own in-flight `rp` call,
then runs cleanup on a token the cancellation cannot reach: panel off,
cover restored only if it started open. A refused `open_cover` (the
night has turned unsafe) is reported as a warning — the cover correctly
stays closed — never as a failure of the flats. This is the service's
existing cleanup shape moved under a cancellable body.

### D10 — Startup, auth, registration

- **The provider answers `tools/list` with no `rp` in sight.** `rp`
  dials providers at startup and fails when one is down (slice 8), and
  the provider connects to `rp` lazily per tool call, so there is no
  cycle. `rusty-photon-rp.service` gains
  `After=rusty-photon-calibrator-flats.service`. The provider's config
  file stays required (`ConditionPathExists`): `mcp_server_url` has no
  sensible default.
- **`/mcp` is guarded by the same `server.auth` and `server.tls` that
  guard `/health`.** `rp`'s registration `auth` is the client-credential
  shape (slice 8), so the two sides match by construction.
- **Transport:** stateless streamable HTTP, `json_response`, the stack
  `bdd-infra`'s `ToolProviderStub` already uses.
- **The registration in `rp`'s config** carries the gate opt-out and
  the dependency list; rp.md's example config ships it, or a fresh
  install comes up with the flats tools gated:

  ```json
  {
    "name": "calibrator-flats",
    "type": "tool_provider",
    "mcp_server_url": "https://localhost:11170/mcp",
    "auth": { "username": "observatory", "password": "secret" },
    "gate": { "train_flats": "none", "take_flats": "none",
              "get_flat_training": "none" },
    "requires_tools": [
      "get_train_info", "get_camera_info", "capture",
      "compute_image_stats", "set_filter", "get_cover_state",
      "close_cover", "open_cover", "calibrator_on", "calibrator_off"
    ]
  }
  ```

## Open items

- **O1 — Two calibrator devices on one train.** A motorized dust cap
  plus a separate light panel needs capability-resolved addressing
  (cover verbs pick the member with a cover, lamp verbs the member with
  a calibrator). Deferred until a real rig needs it; D3's error names
  the limitation.
- **O2 — Rotator-aware and per-filter-brightness flats.** Carried over
  from `calibrator-flats.md` § Future Considerations. The record's
  `brightness` is per filter already; taking flats at the light frames'
  rotator angle is a `take_flats` argument for a later plan.
- **O3 — A new worked example for the DSL.** D13 used the
  service/document pair to show how to choose a workflow's form. The
  pair goes (D2); a replacement example is found later.
- **O4 — Starting flats from `ui-htmx`.** MCP-only (D2) means a UI
  button would call `rp`'s `tools/call`, which `ui-htmx` does for nothing
  today. Not in scope.
- **O5 — Panel aging.** Deliberately not modeled (D5, D7). If retraining
  becomes a chore, a follow-up can add write-back of a corrected
  duration; the record shape admits it.

## Slices

Three PRs, each independently green. Slice 2 depends on slice 1's tools;
slice 3 depends on slice 2 existing so the record never claims two
implementations at once.

### Slice 1 — `rp`: the calibrator joins the train (D3, D4)

- `equipment/trains.rs`: `cover_calibrators[]` ids admitted in
  `devices`; first-only, one-per-train, sharing rules with dotted-path
  errors; the resolved train model gains `calibrator: Option<id>`.
- `mcp/built_in/cover_calibrator.rs`: the five tools take exactly one of
  `calibrator_id` / `train_id`; results gain `calibrator_id` and
  `trains`. Gate classes unchanged.
- `get_train_info` (ungated) — new read in the equipment category.
- `get_camera_info` gains `gain` and `offset` from the device.
- Tests: unit tests for the three train rules and the resolution;
  BDD against OmniSim's CoverCalibrator: "A cover calibrator first in a
  train is accepted", "A cover calibrator after another device fails
  load", "Two calibrators in one train fail load", "A shared calibrator
  reports both trains on close", "`close_cover` by `train_id` closes the
  train's calibrator", "`get_train_info` lists the wheel's filters and
  the calibrator", "`get_camera_info` reports gain and offset".
- Docs: rp.md § Optical Trains (membership, rules, the one-per-train
  exception), § Tools table (six rows), a new § Cover Calibrator Tool Details subsection for the addressing and the `trains` field;
  optical-trains.md decision 1 gets an "amended by" note pointing here;
  the example config puts the calibrator first in the `main` train.

**Verification:** `bazel test //services/rp:bdd` green with OmniSim;
`rp doctor` on the example config clean.

### Slice 2 — `calibrator-flats`: store, server, tools (D1, D5–D10)

- `store.rs`: redb store per D5 (`rp-targets` conventions), unit-tested
  for overwrite, unconverged-writes-nothing, staleness per field, and
  the filterless key.
- `mcp_server.rs`: rmcp server on `/mcp` under the existing
  `server.auth`/`server.tls`; `tools.rs`: `train_flats`, `take_flats`,
  `get_flat_training` per D6–D9, on top of the existing `workflow.rs`
  search (which gains the D8 floor). `config.rs`: `mcp_server_url`,
  `initial_duration`, `tolerance`, `max_iterations`, `server`,
  `service_auth`, `ca_cert` stay; `min_exposure`, `flat_warn_tolerance`,
  `store_path` arrive; `camera_id`, `filter_wheel_id`, `calibrator_id`,
  `filters`, `brightness`, `target_adu_fraction` go, and the loader
  fails loud on any of them naming the field (the slice-5 precedent).
- `routes.rs`: `/runs` and `/status` removed; `/health` stays.
- Packaging: `rusty-photon-rp.service` `After=` ordering (D10).
- Tests: BDD with the service registered in `rp` as a provider, tools
  called through `rp`'s proxy: "The flats tools appear in rp's catalog
  ungated", "`train_flats` records a converged filter and reports an
  unconverged one", "`take_flats` refuses an untrained filter before
  actuating", "`take_flats` refuses a stale record naming the changed
  field", "`take_flats` captures the requested frames and warns on an
  out-of-range median", "A search below `min_exposure` steps the panel
  down", "A cancelled `take_flats` turns the panel off and restores an
  open cover", "A refused `open_cover` after an unsafe transition is a
  warning, not a failure", "`/mcp` requires the service's Basic Auth".
- Docs: `calibrator-flats.md` rewritten (Overview, Architecture, Tools,
  Store, Configuration, Module Structure, Testing); rp.md § Tool
  Provider Registration example gains the registration with its `gate`
  and `requires_tools`; `doctor.md` unchanged (slice 8's joins cover it).

**Verification:** the slice's BDD suite green; on a rig, `train_flats`
on the imaging train at dusk, then `take_flats` from a
`session-runner` document the next evening with the cover found open and
restored.

### Slice 3 — `session-runner` and the record (D1, D2)

- Retire `services/session-runner/workflows/calibrator_flats.json` and
  the tests built on it (`document/corpus.rs`, `catalog_tests.rs`,
  `validate_tests.rs`, `engine/exec_tests.rs`, `flat_calibration.feature`
  and its steps); the shipped documents that remain cover what those
  tests proved, or the tests get a replacement fixture.
- Docs: mcp-sessionless.md D13 (the `calibrator-flats` paragraph is
  replaced by a pointer to D1 here); `calibrator-flats.md`'s "kept on
  purpose" paragraph; `session-runner.md` § Example Documents, the BDD
  equivalence row and the "generalization proof" section;
  `docs/workspace.md`'s service row.

**Verification:** `bazel test //services/session-runner/...` green; no
remaining reference to `calibrator_flats.json` outside the archive.

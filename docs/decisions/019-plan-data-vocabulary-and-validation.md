# ADR-019: The `rp-vocabulary` Crate — Plan-Data Value Types and Cross-Surface Validation

## Status

Accepted (2026-07-23); landing across `feature/rp-targets-p1`
(planetarium target import P1). The value types, the `rp_targets::Target`
and `PlannerTarget` `coord: IcrsCoord` newtype migration, and the
`rp-ephemeris` `From`/`TryFrom` boundary bridge have landed; the
`schema`/`validate` MCP tools follow (see Non-goals).
Full type design: [`docs/crates/rp-vocabulary.md`](../crates/rp-vocabulary.md).
Plan entry: [`planetarium-target-import.md`](../plans/archive/planetarium-target-import.md)
Decision 12.

Origin: reviewing the P1 target store surfaced validation that was
duplicated in some places, split across module/crate boundaries in
others, and missing on a new write path — against a plan API that is
becoming a published, multi-surface contract. Settled interactively with
the project lead over the design thread on 2026-07-23.

## Context

Two problems, both visible in the P1 target-store code.

**1. Plan-data validation was drifting.**

- ICRS coordinate bounds (`ra_hours ∈ [0,24)`, `dec_degrees ∈ [-90,90]`)
  existed twice in agreement — `rp::planner::primitives::validate_icrs`
  and inline in the legacy config-array parser
  `rp::planner::decision::parse_targets_from_value` — and were **missing
  entirely** on the store-write path (`add_target` / `update_target`
  accepted raw `f64`), so a store-backed target could hold coordinates that
  parser rejected. (That parser and the config `targets[]` array it read
  have since been deleted, leaving the store as the sole target source; the
  `IcrsCoord`/`PlannerTarget` validation this ADR establishes is independent
  of it and unaffected by its removal.)
- `Binning`'s round-trip was split across three modules in two crates:
  `Display` in `rp-targets::model`, the `"AxB"` parse (`parse_binning`) in
  `rp::planner::goal_wire`, and a config→planner import reaching *up* into
  the planner from `rp::config::naming_template` to borrow it.
- Exposure `Duration` had two disagreeing string encodings — the store
  wrote humantime-canonical `"5m"` (via `humantime_serde`) while the
  hand-rolled `goal_wire::format_exposure_duration` produced `"300s"` — the same
  value serialized two ways. (Since resolved: the hand-rolled formatter was
  removed and every surface now uses humantime's `"5m"`; exposure remains a
  plain `Duration` in `rp`, outside this crate.)

The common cause: validated plan values were bare primitives (`f64`,
`Duration`) with the rule written *beside* the data, so it could be — and
was — skipped. Calling the validator from more places only defers the next
omission.

**2. The plan API is becoming a published, multi-surface contract.** `rp`
is roughly 20 % of the eventual system. Grading, mosaic, and other tools —
plus more than one UI and the P3 planetarium-bridge — will read and write
plans. The workspace already solves "one validation, many surfaces" once:
the `config.get`/`config.schema`/`config.apply` protocol in
`rusty-photon-config` (one type, a schema generated *from* it via
`schema_for`, a `validate()` returning dotted-path field errors the UI
renders inline, one authoritative server-side gate). Plan data has no such
home.

### Goals

1. **Validation by construction (parse-don't-validate)** for plan value
   types, so the rule cannot be skipped — the invalid state is
   unrepresentable, not merely discouraged.
2. **One home for the shared plan vocabulary**, independent of any single
   consumer, so consumers #2..N attach to the contract rather than to `rp`.
3. **A schema/validate protocol** (a plan-data analogue of config-actions)
   so non-linking surfaces validate identically to the Rust core.
4. **Structurally end the coordinate drift** by making the field type the
   validated type, not by adding more validator calls.

### Non-goals

- Extracting the file-naming template *engine* — it stays in `rp`
  (session-layer glue, one linker). Only its value types move.
- Unifying ASCOM camera **binning**, mount **pointing**, or
  `rp-ephemeris`'s **computed** `IcrsCoord` with the plan value types —
  false cognates (a driver/ASCOM contract, or a `NaN`-capable computed
  astronomy value, versus a validated plan value); see
  [ADR-006](006-typed-physical-quantities-for-mount-pointing.md) and the
  crate doc.
- A shared `rusty-photon-units` driver foundation — deferred to ADR-006's
  own trigger (a third pointing device).
- Building the full protocol out as `rp` **MCP tools** — this ADR establishes
  the crate, the value types, and the newtype migration; the
  `schema`/`validate` MCP tools on `rp` follow. (Plan-data validation has no
  safety-gate reason to be REST — unlike config — so it is MCP, per rp's
  *MCP is the surface* tenet.)

## Options Considered

### Option 1 — Centralize the validator, no new type (do less)

Keep bare `f64`/`Duration`; route every write path through the existing
`validate_icrs`. **Rejected as the end state**: it fixes today's omission
but leaves the field types bypassable, so every future write path is a
fresh chance to forget — exactly how the store-write gap arose.

### Option 2 — Consolidate into existing leaves, no new crate

Put `Binning` in `rp-targets`, `IcrsCoord` validation in
`rp-ephemeris`. **Rejected**: it cannot give `Target` (in `rp-targets`)
*and* `PlannerTarget` (in `rp`) a shared validated coordinate *field*
without an inverted `rp-targets → rp-ephemeris` edge that drags `erfars`
(C) and `tzf-rs` into a plain data store — and it scatters across two
crates the vocabulary the multi-surface protocol wants in one place.

### Option 3 — A `rp-vocabulary` leaf crate (CHOSEN)

A zero-first-party-dep leaf owns the validated value types; both
`rp-targets` and `rp-ephemeris` gain an edge *to* it (never to each
other); `Target`/`PlannerTarget` hold the validated `IcrsCoord` field.

### Option 4 — Several crates (`rp-primitives` + `rp-naming`)

Split the value types and the naming grammar into separate crates.
**Rejected**: mints crate nodes to serve a single linker (`rp`) and a
naming engine with no second consumer — BUILD/lockfile churn for a future
the roadmap does not contain. The naming slice's boundary is staked in the
crate doc for a later split if a pattern-editor surface ever earns it.

### Naming — `rp-vocabulary`, not `rp-primitives` / `rp-common-types` / `rusty-photon-*`

"Vocabulary" names the *purpose* (the shared nouns interfaces exchange to
interoperate — "vocabulary types"), which is itself the gate against the
junk-drawer failure a "-common-types" crate invites. `rp-*` because these
are astrophotography-**domain** nouns the drivers deliberately do not link
(false cognates); `rusty-photon-*` is reserved for domain-neutral plumbing.
Precedent: `rp-mcp-client` — a shared "talk to `rp`" crate linked by the
`calibrator-flats` and `session-runner` services — is `rp-*`.

## Decision

Adopt **Option 3**. Create `crates/rp-vocabulary`, a leaf with no
first-party dependency, holding three validated value types: `IcrsCoord`
(`try_new`, `[0,24)`/`[-90,90]`), `Binning` (`Display`/`FromStr`), and
`FrameType` (`Display`/`FromStr` + `calibration_slug`). Each is
parse-don't-validate with a `#[serde(try_from = …)]`/`FromStr` boundary and
a **feature-gated** `JsonSchema` (the `schema` feature) so the store leaf
stays schemars-free while `rp` projects the vocabulary onto the wire.

Exposure is deliberately **not** one of these types — it stays a
`Duration` with no invariant a `Duration` doesn't already carry (the
"non-zero" rule is a *goal* constraint, already in
`rp_targets::validate_goals`). Its drift is fixed by standardizing on one
`humantime` encoding (deleting `goal_wire::format_exposure_duration`), and its
filename-token codec lives with the naming engine in `rp`. The value is
named **`exposure_duration`** everywhere — unambiguous where bare
"exposure" (a sub-frame count) and "exposure_time" (time of acquisition)
are not.

Consequential decisions settled here:

- **Newtype-field migration.** `rp_targets::Target` changes its coordinate
  field to `coord: IcrsCoord` (private-field newtype), so the compiler
  forces every construction through `try_new`. The planner's
  `PlannerTarget` adopts the same `coord: IcrsCoord` field: its store
  projection (`From<&Target>`) shares the validated coord directly, and
  the `get_next_target` wire nests it as a `coord` object (the alt/az call
  converts to `rp-ephemeris`'s computed coord via the total `From`
  bridge). The coordinate now serializes as a
  nested `coord` object (`{ra_hours, dec_degrees}`) — `#[serde(flatten)]`
  is removed — so one canonical coordinate shape spans the on-disk store
  (redb JSON) and the MCP wire.
- **`rp-catalog` adopts `IcrsCoord`.** `rp_catalog::ResolvedTarget` also
  takes `coord: IcrsCoord` and `rp-catalog` depends on `rp-vocabulary`, so
  one validated coordinate type spans the plan pipeline (catalog → store →
  planner), replacing the parallel bare-`f64` representations that only
  happened to agree. (Chosen over bridging catalog with a `From` at the
  `rp` boundary, which would leave a bare-`f64` rep behind.)
- **`rp-ephemeris`'s `IcrsCoord` is a false cognate — bridged, not
  unified.** It is a *computed* astronomy value: `Epv00`/`Moon98` build it,
  and the panic-safety contract fills it with `NaN` when the host clock
  misbehaves (`sun_position`/`moon_position` return NaN-filled coords
  rather than crashing), on a circular domain where a body can normalise
  exactly onto the `24.0h` seam. A `try_new`-validated newtype cannot hold
  either (`NaN` rejected; half-open `[0,24)` rejects `24.0`). So
  `rp-ephemeris` keeps its own type and depends on `rp-vocabulary` only for
  two boundary conversions: `From<vocab::IcrsCoord>` (total — a validated
  plan coord is always a valid transform input) and
  `TryFrom<ephemeris::IcrsCoord> for vocab::IcrsCoord` (partial — the
  `NaN`/seam cases surface as a `CoordError`, never a panic or a silent
  clamp). This is the computed-vs-plan analogue of the camera-vs-plan
  binning split.
- **`FrameType`'s home is `rp-vocabulary`**, not a top-level `rp` module
  and not `rp-targets` — the feature-gated schema dissolves the
  schemars-purity reason that would have kept it out of the store leaf.
- **The schema/validate protocol lives in `rp`,** parameterized by these
  types: the crate supplies the validating constructors + schema; `rp`
  owns the `schema`/`validate` MCP tools and the constructor-error →
  dotted-`FieldError` mapping.

This design does not conflict with
[ADR-006](006-typed-physical-quantities-for-mount-pointing.md): that ADR is
explicitly scoped to the mount driver's frame-safe pointing math and
disclaims workspace-wide reach (its Non-goals and Resolved Q3). Its Goal 2
(construct-time / deserialize-time invariants) and its `FlipRangeHours`
`serde(try_from)` config newtype are, in fact, the precedent this crate's
value types follow.

## Migration plan

One PR (`feature/rp-targets-p1`), landed at green checkpoints. Every move
is mechanical, few-caller, and unit-test-pinned:

1. `crates/rp-vocabulary` with the three types + their round-trip tests
   (moved from `rp-targets`/`rp`, not rewritten — losing them is the only
   real regression path).
2. `rp-catalog`'s `ResolvedTarget` adopts `IcrsCoord` (its coordinates are
   validatable parse input, so a malformed row rightly fails `try_new`).
   `rp-ephemeris` does **not** re-home its `IcrsCoord` — that computed,
   `NaN`-capable type is a false cognate (above); it depends on the crate
   only for the `From`/`TryFrom` boundary conversions.
3. `rp-targets` depends on the crate (no `schema` feature) for `Binning`/
   `FrameType`; `Target.coord` becomes `IcrsCoord`; `AcquisitionGoal`
   standardizes on `humantime` for its `exposure_duration`.
4. `rp` depends on the crate with `features = ["schema"]`; `PlannerTarget.
   coord` becomes `IcrsCoord`; `goal_wire::parse_binning` and the
   config→planner import are deleted; the store-write validation gap closes
   by construction.

No backwards-compatibility constraint: the store and MCP tools are new in
this same PR with no shipped caller, so the wire/on-disk shapes are chosen
now, not preserved. No new crates.io dependency and no dependency repin;
adding the new workspace *member* still needs the standard `bazel mod tidy`
refresh (Rule 10).

## Consequences

- **The coordinate drift becomes unrepresentable** — no raw-`f64`
  coordinate field survives, and the store-write validation gap closes by
  construction, not by a remembered call.
- **One coordinate type across the plan pipeline** (catalog → store →
  planner) via the `rp-catalog` adoption; `rp-ephemeris`'s computed
  coordinate stays a separate type, bridged at the boundary by
  `From`/`TryFrom`. The store and MCP wire serialize the coordinate as a
  nested `coord` object (`{ra_hours, dec_degrees}`), `#[serde(flatten)]`
  removed — one canonical coordinate shape across the on-disk store and
  the wire — and the in-memory representation gains validation.
- **A published contract crate** future surfaces (UIs, tools, the bridge)
  either link (Rust) or consume as schema (non-linking) — the
  cross-surface single-validation goal.
- **The store leaf stays light** — `rp-targets` depends on `rp-vocabulary`
  without the `schema` feature, keeping it schemars-free.
- **The naming engine sheds its config→planner import** once `Binning`'s
  parse lives with its `Display`.
- **Friction / cost**: read sites move from `.ra_hours` to `.ra_hours()`;
  every coordinate construction routes through `try_new`; the round-trip
  tests move with their types. All mechanical and test-pinned.
- **Driver quantities stay separate** (camera binning, mount pointing); a
  neutral `rusty-photon-units` foundation is anticipated but out of scope
  until a third pointing device (ADR-006's trigger).

## Resolved questions

1. **New crate vs consolidate** — new leaf (`rp-vocabulary`);
   consolidation can't share a validated coordinate field without an
   inverted ERFA edge.
2. **`rp-catalog`** — adopts `IcrsCoord` (full unification), not a bridge:
   its coordinates are validatable parse input.
3. **`rp-ephemeris`** — the opposite call: keeps its own computed
   `IcrsCoord` (a false cognate — `NaN` degradation sentinel + circular
   domain), bridged to the plan type by `From`/`TryFrom`, not unified. The
   computed→plan direction is `TryFrom` because a `From` there would panic
   on the `24.0h` seam or clamp silently.
4. **`FrameType` home** — `rp-vocabulary` with feature-gated schema, not
   `rp-targets` (schemars purity) or a top-level `rp` module.
5. **Naming engine** — stays in `rp`; only its value types move; the
   grammar slice's boundary is staked for a later split.
6. **Drivers** — no camera/mount driver links `rp-vocabulary`; the
   ASCOM-boundary translation is `rp`'s mcp-client seam.

## References

- [`docs/crates/rp-vocabulary.md`](../crates/rp-vocabulary.md) — the crate
  design (the "how"; this ADR is the "why").
- [`docs/crates/rp-targets.md`](../crates/rp-targets.md) — the store; its
  coordinate section is amended by the newtype migration (the bare on-disk
  decimals become a nested `coord` object).
- [`docs/plans/archive/planetarium-target-import.md`](../plans/archive/planetarium-target-import.md)
  — P1, Decision 12.
- [ADR-006](006-typed-physical-quantities-for-mount-pointing.md) — the
  mount-local typed-quantity precedent (parse-don't-validate newtypes) and
  the anticipated `rusty-photon-units` foundation.
- `rusty-photon-config::actions` — the `config.get`/`config.schema`/
  `config.apply` protocol this generalizes to plan data.

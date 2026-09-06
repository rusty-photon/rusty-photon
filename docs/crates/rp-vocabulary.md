# `rp-vocabulary` Crate Design

**Status:** proposed — designed on `feature/rp-targets-p1`, built in the
same PR; the decision record is
[ADR-019](../decisions/019-plan-data-vocabulary-and-validation.md) and the
plan entry is
[Decision 12](../plans/archive/planetarium-target-import.md). This document is the
design detail behind that decision; the Rule-2 updates it triggers in
[`rp-targets.md`](rp-targets.md), [`rp-ephemeris.md`](rp-ephemeris.md),
`rp-catalog` (no dedicated doc), and
[`docs/services/rp.md`](../services/rp.md) land with the code.

A tiny, dependency-light leaf crate holding the **shared, validated
vocabulary** of `rp`'s imaging plan: the small domain value types —
`IcrsCoord`, `Binning`, `FrameType` — that `rp`, the crates
it is built from (`rp-targets`, `rp-ephemeris`), and every surface that
talks to `rp` about plans must agree on. Each type is
*parse-don't-validate*: a value that exists is valid by construction, and
that one constructor is the single validator every surface shares.

This is a workspace library, not a service, and it holds **no logic** —
no store, no template engine, no ephemeris math, no protocol tools.
It holds *nouns*: the validated representations those layers exchange. It
is the plan-side analogue of what
[`rusty-photon-config`](../../crates/rusty-photon-config) is for driver
config — a shared contract crate, not a grab-bag of utilities.

## Why this crate exists

Two forces produced it, both surfaced while reviewing the P1 target
store.

**1. Validation was drifting across surfaces.** The same rule was written
more than once and — worse — skipped in places:

- ICRS coordinate bounds (`ra_hours ∈ [0,24)`, `dec_degrees ∈ [-90,90]`)
  existed twice in agreement (`rp::planner::primitives::validate_icrs`,
  and inline in `rp::planner::decision::parse_targets_from_value`) and
  were **missing entirely** on the store-write path (`add_target` /
  `update_target` accepted raw `f64`), so a store-backed target could
  hold coordinates the legacy config-array path rejects.
- `Binning`'s round-trip was split across three modules in two crates:
  `Display` in `rp-targets`, the `"AxB"` parse (`parse_binning`) in
  `rp::planner::goal_wire`, and a config→planner import reaching *up* into
  the planner from `rp::config::naming_template` to borrow it.
- Exposure `Duration` had two divergent string encodings — a hand-rolled
  `"300s"` formatter (`goal_wire::format_exposure_duration`) versus the store's
  humantime-canonical `"5m"` (`AcquisitionGoal`'s `humantime_serde`) — the
  same value serialized two ways. (Since resolved: the hand-rolled formatter
  was removed and every surface now uses humantime, so `"5m"` is the one
  encoding; exposure stays a plain `Duration` in `rp`, not a vocabulary type.)

The fix is not "call the validator from more places" (that only defers
the next omission); it is to **make the invalid state unrepresentable**,
so a validator can no longer be forgotten. For the two values with a real
invariant or structure — coordinates and binning — that means a *type*
with a home every layer can depend on. (Exposure is the exception: it has
no invariant a `Duration` doesn't already carry, so it stays a `Duration`
standardized on one `humantime` encoding — see
[Not a type: exposure](#not-a-type-exposure-duration).)

**2. The plan API is becoming a published, multi-surface contract.**
`rp` is ~20 % of the eventual system. Grading, mosaic, and other tools —
plus more than one UI — will all read and write plans. The way
rusty-photon already solves "one validation, many surfaces" is the
`config.get`/`config.schema`/`config.apply` protocol in
`rusty-photon-config`: one type, a schema *generated from* that type
(`schema_for`), a `validate()` returning dotted-path field errors the UI
renders inline, and one authoritative server-side gate. Extending that
pattern to plan data (targets, goals, naming patterns — see
[Schema + validate protocol](#schema--validate-protocol-the-decision-1-role))
needs a crate that owns the *typed vocabulary + its schema + its
constructor validation*. That is this crate.

## Scope

In scope — validated domain value types and nothing else:

- **`IcrsCoord`** — a J2000/ICRS pointing, `try_new`-validated to
  `ra_hours ∈ [0,24)`, `dec_degrees ∈ [-90,90]`.
- **`Binning`** — the `(x, y)` frame-binning pair; `Display` as `"2x2"`,
  `FromStr` parsing `"AxB"`. The `(filter, binning, exposure_duration)`
  quota-key dimension.
- **`FrameType`** — `Light | Dark | Flat | Bias`, the capture intent;
  `Display`/`FromStr`, plus `calibration_slug()` (the reserved
  `dark`/`flat`/`bias` bucket slug).

Each type derives `Serialize`/`Deserialize` (validating on the way in),
and — behind the [`schema` feature](#feature-flags) — `JsonSchema` with
its constraint encoded, so the generated schema is a true projection of
the validator.

Out of scope — owned elsewhere, listed so the boundary is explicit:

- **The target store** ([`rp-targets`](rp-targets.md)) — redb, CRUD,
  migration. It *depends on* this crate for `Binning`/`FrameType`/
  `IcrsCoord`; it is not folded in here.
- **Exposure** — a plain `Duration`, not a value type here (its only wire
  form is `humantime`, and its filename codec lives with the naming
  engine). See [Not a type: exposure](#not-a-type-exposure-duration).
- **Ephemeris math** ([`rp-ephemeris`](rp-ephemeris.md)) — `alt_az`,
  sidereal time, twilight. It keeps its **own** computed `IcrsCoord` (a
  `NaN`-capable false cognate — see below) and depends on this crate only
  for the `From`/`TryFrom` boundary conversions to the plan value.
- **The file-naming template engine** (`rp::config::naming_template`:
  `CompiledTemplate`, `TOKENS`, `parse_segments`, `check_unambiguous`) —
  stays in `rp`. Only the grammar's *value types* live here. See
  [Naming: the deferred slice](#naming-the-deferred-slice).
- **The schema + validate protocol MCP tools** and the dotted
  `FieldError` mapping — live in `rp` (parameterized by these types).
- **ASCOM driver quantities** — camera `BinX`/`BinY`, mount
  `RightAscension`/`Declination`, the shutter `light: bool`. These are a
  *false cognate*, not this vocabulary. See
  [Not this crate](#not-this-crate-false-cognates).
- **ADR-006 mount-local typed quantities** (`MechHa`/`Ra`/`Dec`, encoder
  ticks) — frame-safe *pointing math* inside the mount driver, a
  different concern from plan-data values. See
  [Relationship to ADR-006](#relationship-to-adr-006-and-the-bare-decimals-decision).
- **`rp-ephemeris`'s computed `IcrsCoord`** — a computed, `NaN`-capable
  astronomy value on a circular domain, not a validated plan value; kept
  separate and bridged by `From`/`TryFrom`. See
  [Not this crate](#not-this-crate-false-cognates).

## Crate boundary and dependency graph

`rp-vocabulary` is a leaf with **zero first-party dependencies**, so no
dependency cycle is structurally possible. Both `rp-targets` and
`rp-ephemeris` — today independent sibling leaves — gain an edge *to* it,
never to each other.

```
                    rp-vocabulary  (leaf: serde, thiserror, [schema] schemars)
                    IcrsCoord::try_new   Binning: FromStr   FrameType
                        ▲          ▲            ▲
        ┌───────────────┘          │            └───────────────┐
   rp-ephemeris              rp-targets                     rp-catalog
   (keeps erfars/tzf-rs +    (keeps redb; Target,           (ResolvedTarget
    own computed IcrsCoord;  AcquisitionGoal hold            .coord: IcrsCoord;
    From/TryFrom to vocab)   vocab types)                    depends on vocab)
        ▲                          ▲                             ▲
        └──────────────────────────┼─────────────────────────────┘
                              services/rp
        Target & PlannerTarget hold IcrsCoord; depends on rp-vocabulary
        with `features = ["schema"]`; owns the naming engine + the
        schema/validate protocol MCP tools.
```

The reason the value must live *below* both consumers is
[decision 3](#the-newtype-field-migration-decision-3): for `Target`
(in `rp-targets`) **and** `PlannerTarget` (in `rp`) to hold a validated
`IcrsCoord` field, the type has to sit under both — and putting it in
`rp-ephemeris` instead would force `rp-targets → rp-ephemeris`, dragging
the `erfars` C library and the `tzf-rs` timezone database into a plain
data store. A dependency-light vocabulary leaf avoids that entirely.

## The types

### `IcrsCoord` — validated pointing

```rust
/// A J2000 mean equator/equinox (ICRS) pointing, valid by construction.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(try_from = "IcrsCoordWire", into = "IcrsCoordWire")]
pub struct IcrsCoord {
    ra_hours: f64,     // private: the only way to a value is try_new
    dec_degrees: f64,
}

impl IcrsCoord {
    /// `ra_hours ∈ [0, 24)`, `dec_degrees ∈ [-90, 90]`.
    pub fn try_new(ra_hours: f64, dec_degrees: f64) -> Result<Self, CoordError>;
    pub fn ra_hours(&self) -> f64;
    pub fn dec_degrees(&self) -> f64;
}

pub enum CoordError { RaOutOfRange { ra_hours: f64 }, DecOutOfRange { dec_degrees: f64 } }
```

Fields are **private** — that is what makes the invalid state
unrepresentable (as `TargetSlug` already does with its inner `String`).
The serialized form is a nested `coord` object:
`#[serde(try_from = "IcrsCoordWire")]` where
`IcrsCoordWire { ra_hours, dec_degrees }` is the two-key inner shape, and
the `TryFrom` runs `try_new` — so deserializing a bad row or bad JSON
*fails* rather than smuggling an out-of-range value past the constructor.
The containing `Target`/`ResolvedTarget` no longer `#[serde(flatten)]` the
coordinate, so it serializes as
`"coord": { "ra_hours": <f64>, "dec_degrees": <f64> }` — one canonical
shape on disk and on the MCP wire alike. Read sites migrate from
`.ra_hours` to `.ra_hours()`.

### `Binning` — completed round-trip

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, /* serde, [schema] */)]
#[display("{x}x{y}")]
#[serde(into = "String", try_from = "String")]
pub struct Binning { pub x: u8, pub y: u8 }

impl FromStr for Binning { /* "AxB" → Binning; errors on non-"AxB" or non-u8 */ }
```

`Binning`'s fields stay public: any `u8 × u8` is shape-valid, so there is
no bound to protect and no bypass to close — the fix is simply to put the
`FromStr` **next to** the `Display` it inverts, retiring
`goal_wire::parse_binning` and the config→planner import that borrowed it.
Serde goes through the same string
(`#[serde(into = "String", try_from = "String")]` delegating to
`Display`/`FromStr`), so the redb store, the MCP wire, config, and the
filename token all share the one canonical `"AxB"` encoding — the same
one-shape doctrine `IcrsCoord` applies to coordinates. (The `schema`
feature's `JsonSchema` impl is manual — the derive can't follow
`into`/`try_from` to the string shape.)
(The naming-token grammar `\d+x\d+` in `rp` is laxer than `u8`; that
mismatch is pre-existing and tracked as a follow-up, not fixed here.)

### `FrameType` — capture intent

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, /* serde, [schema] */)]
pub enum FrameType { Light, Dark, Flat, Bias }

impl FrameType {
    /// The reserved `{target}` bucket for a calibration frame with no
    /// explicit target: lowercased type. `None` for `Light`.
    pub fn calibration_slug(&self) -> Option<&'static str>;
}
impl Display for FrameType   { /* "Light" | "Dark" | "Flat" | "Bias" */ }
impl FromStr  for FrameType  { /* exact, case-sensitive inverse of Display */ }
```

This is where `FrameType` finally belongs. It was buried in
`rp::config::naming_template` yet consumed as a cross-cutting domain type
(the `{frame_type}` token, the `capture` MCP param, the exposure sidecar
document). Earlier analysis weighed "leave it in a top-level `rp` module"
against "push it into `rp-targets`," and rejected the latter only to keep
`schemars` out of the store leaf. The vocabulary crate dissolves that
trade-off: `FrameType` lives here with a **feature-gated** `JsonSchema`,
`rp-targets` depends on this crate *without* the `schema` feature (so it
stays schemars-free), and `rp` turns the feature on.

### Not a type: exposure (`Duration`)

Exposure is deliberately **not** a value type here — it stays a
`std::time::Duration`. Unlike coordinates (a real range invariant) or
binning (a structured pair), a duration has no invariant `Duration`
doesn't already carry; the only candidate rule, "non-zero," is a *goal*
constraint already enforced by `rp_targets::validate_goals`, not a
property of the value. A newtype would wrap a single std type for no gain
and tax every arithmetic site with an `.as_duration()`.

The drift it caused is fixed by **one encoding, not a type**: standardize
on `humantime` everywhere (`#[serde(with = "humantime_serde")]` on the
field), deleting the hand-rolled `goal_wire::format_exposure_duration`. The
filename-token form (`humantime` with spaces stripped, so the path
component stays space-free and regex-clean) lives with the naming engine
in `rp` — its only consumer. So exposure touches this crate **nowhere**.

The value is named **`exposure_duration`** everywhere: the token
`{exposure_duration}`, the `AcquisitionGoal.exposure_duration` field, the
`(filter, binning, exposure_duration)` quota key. Bare "exposure" is
ambiguous (in astrophotography "an exposure" is a *sub-frame* — a count),
and "exposure_time" is ambiguous the other way (the *time of
acquisition*); "exposure_duration" is the one reading that can't be
misread. (The rename across the naming engine, `AcquisitionGoal`, and the
docs is part of the consumer migration.)

## Relationship to the `rp-targets.md` "bare decimals" decision

[`rp-targets.md`](rp-targets.md#coordinates-validated-icrscoord-nested-coord-object)
documents a deliberate decision to store `ra_hours`/`dec_degrees` as bare
`f64`. That is the decision this design touches; it is **consistent with
it**, and the reconciliation is worth recording so it travels with the
code.

That section leans on
[ADR-006](../decisions/006-typed-physical-quantities-for-mount-pointing.md)
for support, so it is worth being precise about what ADR-006 does and does
not say here — it is *not* a constraint this crate has to work around:

- **ADR-006 has no jurisdiction over plan coordinates, and doesn't claim
  any.** It is explicitly scoped to the Star Adventurer mount's pointing
  math (Non-goals: *"Rewriting other services … workspace-wide adoption is
  a later, separate question"*; Resolved Q3: *"Workspace-wide adoption —
  out of scope here"*). Its `MechHa`/`Ra`/`Dec` types exist to make
  mechanical-vs-celestial *frame* mix-ups compile errors inside that
  driver. `IcrsCoord` carries no frame distinction; it is a *plan-data
  value* with a range check — a different type for a different layer.
- **If anything, ADR-006 is the precedent *for* this pattern.** Its Goal 2
  is *"construct-time / deserialize-time invariants (parse-don't-validate)"*,
  and its `FlipRangeHours` config newtype is a private-field `f64` with
  `#[serde(try_from = "f64", into = "f64")]` and a range-checking `TryFrom`
  — exactly `IcrsCoord`'s shape. The `rp-targets.md` "bare decimals" choice
  was an *alignment* argument (match `ResolvedTarget`/`IcrsCoord`, which
  were bare `f64`), not a prohibition on validating newtypes.
- **The "bare decimals" goals are preserved, and alignment *improves*.**
  That decision had two aims: don't adopt the ADR-006 mount types for
  plan data (honored — `IcrsCoord` is not one), and stay aligned with
  `rp_catalog::ResolvedTarget` and `rp_ephemeris::IcrsCoord`. The catalog
  and plan coordinates were parallel bare-`f64` reps that merely happened
  to match; making `IcrsCoord` the one shared *plan* value type unifies
  them (catalog → store → planner). `rp_ephemeris::IcrsCoord` stays a
  separate type by design — a computed, `NaN`-capable false cognate
  bridged by `From`/`TryFrom` (see
  [Not this crate](#not-this-crate-false-cognates)) — so the coupling that
  remains is an explicit typed boundary, not an accidental look-alike.
  Alignment goes *up*, not down.
- **The serialized coordinate is a nested `coord` object.** `IcrsCoord`
  serializes as `"coord": { "ra_hours": <f64>, "dec_degrees": <f64> }`
  rather than flat top-level keys — the store `Target` and catalog
  `ResolvedTarget` no longer `#[serde(flatten)]` it, giving one canonical
  coordinate shape across the on-disk redb store and the MCP wire. The
  values inside that object are still bare decimals (the part of the
  decision about *not* adopting typed quantities is untouched); the
  *in-memory* representation gains validation and the serialized form
  gains the `coord` nesting.

### `rp-catalog` adopts `IcrsCoord` (settled — ADR-019)

`rp_catalog::ResolvedTarget` also adopts `IcrsCoord`
(`ResolvedTarget.coord: IcrsCoord`) and `rp-catalog` gains a dependency on
`rp-vocabulary`. This is the de-drift across the *plan* pipeline: **one**
validated coordinate type catalog → store → planner, replacing the
parallel bare-`f64` representations that previously only *happened* to
agree. The alternative considered and rejected was bridging catalog with a
`From`/`try_new` at the `rp` boundary (a smaller diff, but it would leave a
bare-`f64` rep in the workspace). `rp-catalog` stays a light leaf — it
gains only the `rp-vocabulary` edge, no other new dependency.

`rp-ephemeris` is deliberately *not* on that unified path — its
`IcrsCoord` is a computed, `NaN`-capable false cognate (see
[Not this crate](#not-this-crate-false-cognates)) that a validated newtype
cannot represent, so it is bridged by `From`/`TryFrom` at the boundary
rather than replaced. Validated plan input and computed astronomy output
are genuinely different concerns; the typed bridge makes the seam explicit
instead of pretending one type serves both.

## The newtype-field migration (decision 3)

The coordinate drift is prevented structurally by changing the *field
type*, not by adding more calls to a validator:

- `rp_targets::Target` — `ra_hours: f64, dec_degrees: f64` → `coord: IcrsCoord`.
- `rp::planner::decision::PlannerTarget` — same.

With no raw-`f64` coordinate field left, every construction — today's
`add_target`/`update_target`, `parse_targets_from_value`, and any future
write path — is *forced* through `IcrsCoord::try_new` by the compiler. The
`update_target` line that did `target.ra_hours = v` no longer type-checks;
it becomes `target.coord = IcrsCoord::try_new(...)?`. Serde now nests the
coordinate as `"coord": { "ra_hours": <f64>, "dec_degrees": <f64> }`
(above) instead of `#[serde(flatten)]`-ing it to the top level, so the
redb rows and the MCP JSON both carry one canonical `coord` object. This
is not a backwards-compatibility exercise — the
store and tools are new in this PR with no shipped caller — it is doing
the change *while it is still free*, before the wire ossifies into a
contract.

## Schema + validate protocol (the decision-1 role)

`rp-vocabulary` is the *enabler* of a plan-data analogue of the
`config-actions` protocol; it does not own the protocol. The division:

- **This crate provides**: the validated constructors (the single
  validator, returning typed `thiserror` errors) and — behind the
  `schema` feature — the `JsonSchema` derives with constraints encoded
  (`ra_hours` min/max, `Binning`/`FrameType` shapes), so the
  published schema cannot drift from the validator.
- **`rp` provides**: the `schema`/`validate` MCP tools for targets, goals,
  and naming patterns, and the mapping from a constructor `Err` to a
  dotted-path `FieldError { path, msg }` (e.g.
  `{ path: "ra_hours", msg: "ra_hours 25 is outside [0, 24)" }`) — the same shape
  `config-actions` returns, so a UI renders the error next to the field.
- **Surfaces consume**: Rust linkers (`rp`, future tools) get identical
  validation by construction; non-linking surfaces (the ui-htmx BFF, a
  browser form, a future bridge) render forms from the published schema
  and defer to `rp`'s constructor as the authoritative gate. This keeps
  ui-htmx a thin renderer (the "UI contains zero application logic" tenet)
  exactly as it already renders driver-config forms from `config.schema`.

This cross-crate decision is recorded in
[ADR-019](../decisions/019-plan-data-vocabulary-and-validation.md); this
crate doc is the design detail behind it.

## Naming: the deferred slice

The file-naming template *engine* stays in `rp` — it is session-layer glue
(regex + chrono + `SessionConfig`) with exactly one linker and none on the
horizon (ui-htmx passes patterns through as opaque strings; a future
planetarium-bridge creates targets over MCP and never renders filenames).
What *could* migrate here later is the thin reusable slice a
pattern-editor surface would want — a `NamingPattern` newtype wrapping the
token grammar and `validate_pattern` — leaving `CompiledTemplate` (render)
in `rp`. That is **not** in this PR; the boundary is staked so it can move
cleanly when a real second consumer (a pattern editor) exists. Until then,
`Binning`/`FrameType` moving here already lets the engine drop its
config→planner import.

## Not this crate: false cognates

Several types share a *shape* with these plan values but a different
*concern*, so they stay separate. The ASCOM camera/mount drivers are
**not** consumers and their look-alike quantities are **not** this
vocabulary:

- Camera **binning** (`bin: u32`, `BinX`/`BinY`) is a *symmetric hardware
  factor* validated against the sensor's `supported_bins`, with
  `can_asymmetric_bin()` hardcoded `false` — a driver/ASCOM contract, `u8`
  at Alpaca and `u32` at the SDK FFI. `Binning{x,y}` is an
  asymmetric-capable *plan-data quota key* with no hardware validation.
  They share only the word.
- `start_exposure(duration, light: bool)` is a 2-way shutter boolean, not
  the 4-way `FrameType` intent — there is no Flat/Bias concept at the
  driver layer.
- Mount **pointing** (`RightAscension`/`Declination`) is received by a
  telescope driver as f64 over the ASCOM trait and immediately wrapped in
  that driver's *own* frame-safe types (ADR-006's `Ra`/`Dec`/`MechHa`/…),
  which model mechanical-safety frame distinctions `IcrsCoord` does not —
  and the driver re-validates against its mechanical envelope regardless,
  because a safety-critical device cannot trust upstream validation. The
  established mount/rotator drivers link **no** plan-domain crate (they
  even wrap `erfars` directly rather than share `rp-ephemeris`); a driver
  linking `rp-vocabulary` would invert that (driver → rp-domain) for a
  transient boundary wrapper.
- **`rp-ephemeris`'s computed `IcrsCoord`** (in-workspace, not a driver) is
  a *computed astronomy output*: `Epv00`/`Moon98` build it, and the
  panic-safety contract fills it with `NaN` when the host clock misbehaves
  (`sun_position`/`moon_position` degrade rather than crash), on a circular
  domain where a body can normalise onto the `24.0h` seam. A
  `try_new`-validated newtype cannot hold `NaN` or `24.0` (half-open
  `[0,24)`), so unifying the two would force a `Result` at every computed
  construction and turn the structurally panic-free ephemeris into one that
  panics on the seam (if unwrapped) or reports a spurious "no position" (if
  mapped to `None`). Instead `rp-ephemeris` keeps its own type and links
  `rp-vocabulary` only for two boundary conversions: `From<vocab::IcrsCoord>`
  (plan → computed, total) and `TryFrom<ephemeris::IcrsCoord> for
  vocab::IcrsCoord` (computed → plan, partial — the `NaN`/seam cases surface
  as a `CoordError`). This is the same validated-input-vs-computed-output
  split that keeps camera binning separate from plan binning, one layer up.

No camera/mount service links `rp-vocabulary`; the translation from plan
`Binning`/`IcrsCoord` to Alpaca `BinX`/`BinY` and `RightAscension`/
`Declination` is `rp`'s mcp-client seam. This is why the crate is
`rp-*` (astro-**domain** vocabulary) and not `rusty-photon-*` (which is
reserved for domain-**neutral** plumbing every service including the
drivers links — transport, TLS, config, lifecycle, i18n). If a *third*
pointing device ever earns it (ADR-006's own trigger — the rotator and
Star Adventurer are two), the coordinate/angle sharing that belongs
*across drivers* is a neutral `rusty-photon-units` foundation their
frame-safe types build on — never `IcrsCoord` reaching down into a driver.

## Feature flags

```toml
[features]
default = []
schema  = ["dep:schemars"]   # JsonSchema derives on every type
```

`schema` is off by default so a consumer that only needs the DTOs +
validation (e.g. `rp-targets`, or a future thin client) does not inherit
`schemars`. `rp` enables it for the schema/validate protocol. This is the
one lever that keeps the store leaf as light as it is today while still
letting `rp` project the vocabulary onto the wire.

## Dependencies

| Crate | Purpose |
|---|---|
| `serde` / `serde_json` | value (de)serialization, validating on input |
| `thiserror` | the constructor error enums (`CoordError`, `BinningParseError`, `FrameTypeParseError`) |
| `derive_more` | `Display` derives (`Binning`, `FrameType`) |
| `schemars` *(optional, `schema`)* | `JsonSchema` for the wire projection |

No new crates.io dependency is introduced — every one is already in the
workspace. No `MODULE.bazel.lock` repin is required for the crate's deps;
adding a new workspace member still needs the standard `bazel mod tidy`
refresh per [Rule 10](../workspace.md#bazel-primary-ci-gate).

## Module layout

```
crates/rp-vocabulary/src/
├── lib.rs        # crate root + re-exports; #![deny(unsafe_code)]
├── coord.rs      # IcrsCoord, CoordError, IcrsCoordWire
├── binning.rs    # Binning + FromStr
└── frame_type.rs # FrameType + calibration_slug
```

Crate-root attributes match the sibling crates
(`#![cfg_attr(coverage_nightly, feature(coverage_attribute))]`,
`#![deny(unsafe_code)]`).

## Testing

Each type's round-trip is pinned in-crate, and — critically — the
existing round-trip tests **move here with the code they cover**, so the
relocation cannot silently regress the render/parse contract:

- `IcrsCoord` — `try_new` accepts in-range / rejects each out-of-range
  bound; `serde` round-trips through the two-key `coord` wire and
  *rejects* an out-of-range wire value on deserialize.
- `Binning` — `Display`/`FromStr` round-trip; `FromStr` rejects non-`AxB`
  and non-`u8`.
- `FrameType` — `Display`/`FromStr` round-trip every variant (the moved
  `frame_type_round_trips_every_variant` test); `calibration_slug`.

Tests use `.unwrap()`/`.unwrap_err()` per
[testing.md](../skills/testing.md), scoped by the usual `#[allow(...)]` on
the test module.

## In this PR vs deferred

**In this PR:** the crate and its three types with validation +
feature-gated schema; `Binning`/`FrameType` moved off `rp-targets`/`rp`;
the new validated `IcrsCoord` living here, with `rp-ephemeris` keeping its
own computed `IcrsCoord` and gaining the `From`/`TryFrom` bridges to it
(not re-homing — the computed type is a false cognate); `rp-catalog`'s
`ResolvedTarget` adopting the validated `IcrsCoord`; the newtype-field
migration of `Target` and `PlannerTarget`; the coordinate-validation gap
on the store-write path closed by construction; the round-trip tests moved
with their types.

**Deferred:** the `NamingPattern` grammar slice (stays in `rp` until a
pattern editor exists).

**Landed since:** `rp`'s `get_plan_schema` / `validate_plan` MCP tools
and the `FieldError` mapping, built on this crate's validating
constructors and `schema`-gated derives (rp.md § Plan schema and
validation). There is no `apply` counterpart — the existing write tools
(`add_target`, `update_target`, `set_goals`) are the apply step, and
they now share their rules with `validate_plan` rather than restating
them.

## Decision rationale (alternatives considered)

- **`rp-vocabulary` over `rp-primitives` / `rp-common-types`.**
  "Primitives" reads as *language* primitives (int/bool), underselling
  that these are validated domain value types. "Common-types" is
  semantically empty and invites the crate to become a workspace junk
  drawer — the opposite of a disciplined, published contract. "Vocabulary"
  names the *purpose* (the shared nouns many interfaces exchange to
  interoperate — the term-of-art "vocabulary types"), which is itself the
  gate against scope creep: a random helper isn't vocabulary.
- **`rp-*` over `rusty-photon-*`.** The workspace convention is
  `rusty-photon-*` for domain-*neutral* plumbing every service links
  (drivers included), `rp-*` for astrophotography-*domain* crates. These
  are domain nouns the drivers deliberately do **not** link (false
  cognates). Precedent: `rp-mcp-client` — a shared "talk to `rp`" contract
  linked by multiple *services* (`calibrator-flats`, `session-runner`) —
  is `rp-*`, so a shared plan-vocabulary linked by `rp` and future clients
  is the same category.
- **One vocabulary crate over several (`rp-primitives` + `rp-naming`).**
  Every value type has exactly one linking consumer today (`rp`); the
  naming *engine* has no second consumer on the roadmap. Minting multiple
  crate nodes now to encode a future the roadmap doesn't contain adds
  BUILD files and lockfile churn for no consumer they uniquely serve. One
  leaf, with the naming slice's boundary staked for later.
- **Consolidate into existing leaves vs a new crate at all.** A pure
  "no new crate" consolidation (put `Binning` in `rp-targets`,
  `IcrsCoord` in `rp-ephemeris`) fixes today's drift but cannot give
  `Target` *and* `PlannerTarget` a shared validated coordinate field
  without an inverted `rp-targets → rp-ephemeris` (ERFA) edge — and it
  scatters the plan vocabulary that the multi-surface schema/validate
  protocol wants in one place. The leaf crate is what makes the
  newtype-field guarantee (decision 3) and the protocol (decision 1) both
  buildable.
```
# `rp-vocabulary` Crate Design

A tiny, dependency-light leaf crate holding the **shared, validated
vocabulary** of `rp`'s imaging plan: the small domain value types —
`IcrsCoord`, `Binning`, `FrameType` — that `rp`, the crates
it is built from (`rp-targets`, `rp-catalog`, `rp-ephemeris`), and every
surface that talks to `rp` about plans must agree on. Each type is
*parse-don't-validate*: a value that exists is valid by construction, and
that one constructor is the single validator every surface shares.

This is a workspace library, not a service, and it holds **no logic** —
no store, no template engine, no ephemeris math, no protocol tools.
It holds *nouns*: the validated representations those layers exchange. It
is the plan-side analogue of what
[`rusty-photon-config`](../../crates/rusty-photon-config) is for driver
config — a shared contract crate, not a grab-bag of utilities.

## Why this crate exists

Two forces shape it.

**1. A rule written beside its data drifts.** When a plan value is a
bare primitive and its rule lives in a validator next to it, the rule
gets written more than once and skipped somewhere: the ICRS bounds
checked on one write path and missed on another, `Binning`'s `Display`
in one crate and its `"AxB"` parse in another, one exposure serialized
as `"300s"` by one surface and as `"5m"` by the next.

The fix is not "call the validator from more places" (that only defers
the next omission); it is to **make the invalid state unrepresentable**,
so a validator cannot be forgotten. For the two values with a real
invariant or structure — coordinates and binning — that means a *type*
with a home every layer can depend on. (Exposure is the exception: it has
no invariant a `Duration` doesn't already carry, so it stays a `Duration`
standardized on one `humantime` encoding — see
[Not a type: exposure](#not-a-type-exposure-duration).)

**2. The plan API is a published, multi-surface contract.**
`planetarium-bridge` writes plans over MCP, and grading, mosaic, and
other tools — plus more than one UI — are designed to read and write
them too. The way rusty-photon already solves "one validation, many
surfaces" is the setup pages' `ConfigurableDriver` in
`rusty-photon-config`: one type, a schema
*generated from* that type (`schema_for`), a `validate()` returning
dotted-path field errors the page renders inline, and one authoritative
server-side gate. Extending that
pattern to plan data (targets, goals, naming patterns — see
[Schema + validate protocol](#schema--validate-protocol))
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
  migration. It *depends on* this crate for `Binning` and `IcrsCoord`;
  it is not folded in here.
- **Exposure** — a plain `Duration`, not a value type here (its only wire
  form is `humantime`, and its filename codec lives with the naming
  engine). See [Not a type: exposure](#not-a-type-exposure-duration).
- **Ephemeris math** ([`rp-ephemeris`](rp-ephemeris.md)) — `alt_az`,
  sidereal time, twilight. It keeps its **own** computed `IcrsCoord` (a
  `NaN`-capable false cognate — see below) and depends on this crate only
  for the `From`/`TryFrom` boundary conversions to the plan value.
- **The file-naming template engine** (`rp::config::naming_template`:
  `CompiledTemplate`, `Token`, `parse_segments`, `check_unambiguous`) —
  lives in `rp`. Only the grammar's *value types* live here. See
  [Naming: the deferred slice](#naming-the-deferred-slice).
- **The schema + validate protocol MCP tools** and the dotted
  `FieldError` mapping — live in `rp` (parameterized by these types).
- **ASCOM driver quantities** — camera `BinX`/`BinY`, mount
  `RightAscension`/`Declination`, the shutter `light: bool`. These are a
  *false cognate*, not this vocabulary. See
  [Not this crate](#not-this-crate-false-cognates).
- **The Star Adventurer GTi driver's typed quantities** (`MechHa`/`Ra`/
  `Dec`, encoder ticks) — frame-safe *pointing math* inside the mount
  driver, a different concern from plan-data values. See
  [Not this crate](#not-this-crate-false-cognates).
- **`rp-ephemeris`'s computed `IcrsCoord`** — a computed, `NaN`-capable
  astronomy value on a circular domain, not a validated plan value; kept
  separate and bridged by `From`/`TryFrom`. See
  [Not this crate](#not-this-crate-false-cognates).

## Crate boundary and dependency graph

`rp-vocabulary` is a leaf with **zero first-party dependencies**, so no
dependency cycle is structurally possible. `rp-targets`, `rp-ephemeris`
and `rp-catalog` each have an edge *to* it and none to each other.

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

The value lives *below* its consumers because of
[the coordinate fields](#the-coordinate-fields): for `Target`
(in `rp-targets`) **and** `PlannerTarget` (in `rp`) to hold a validated
`IcrsCoord` field, the type has to sit under both — and putting it in
`rp-ephemeris` instead would force `rp-targets → rp-ephemeris`, dragging
the `erfars` C library and the `tzf-rs` timezone database into a plain
data store. A dependency-light vocabulary leaf avoids that entirely.

`rp_catalog::ResolvedTarget` holds the same validated `IcrsCoord`, so
one coordinate type runs catalog → store → planner; bridging the catalog
with a `From`/`try_new` at the `rp` boundary instead would leave a
second, bare-`f64` representation in the workspace. `rp-catalog`'s one
first-party edge is to `rp-vocabulary`. `rp-ephemeris` is not on that
path: its computed coordinate is a false cognate, bridged at the
boundary (see [Not this crate](#not-this-crate-false-cognates)).

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
The containing `Target`, `ResolvedTarget` and `PlannerTarget` do not
`#[serde(flatten)]` the coordinate, so it serializes as
`"coord": { "ra_hours": <f64>, "dec_degrees": <f64> }` — one canonical
shape on disk and on the MCP wire alike. The values inside are plain
decimal hours and degrees: the type adds validation, not units. Read
sites call `.ra_hours()` / `.dec_degrees()`.

### `Binning` — the `"AxB"` round-trip

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, /* serde, [schema] */)]
#[display("{x}x{y}")]
#[serde(into = "String", try_from = "String")]
pub struct Binning { pub x: u8, pub y: u8 }

impl FromStr for Binning { /* "AxB" → Binning; errors on non-"AxB" or non-u8 */ }
```

`Binning`'s fields stay public: any `u8 × u8` is shape-valid, so there is
no bound to protect and no bypass to close — the `FromStr` sits **next
to** the `Display` it inverts, so no other module keeps a parse of its
own. Serde goes through the same string
(`#[serde(into = "String", try_from = "String")]` delegating to
`Display`/`FromStr`), so the redb store, the MCP wire, config, and the
filename token all share the one canonical `"AxB"` encoding — the same
one-shape doctrine `IcrsCoord` applies to coordinates. (The `schema`
feature's `JsonSchema` impl is manual — the derive can't follow
`into`/`try_from` to the string shape.)
(The naming-token shape `\d+x\d+` in `rp` is laxer than `u8`: a
filename whose binning factor exceeds 255 matches the shape, parses back
with no binning, and counts toward no goal.)

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

`FrameType` is a cross-cutting domain type — the `{frame_type}` token,
the `capture` MCP parameter, the exposure sidecar document — so it lives
with the other plan nouns rather than inside `rp`'s naming engine. Its
`JsonSchema` is **feature-gated** like every type's here, so a consumer
that needs no schema (`rp-targets`) stays schemars-free while `rp` turns
the feature on.

### Not a type: exposure (`Duration`)

Exposure is deliberately **not** a value type here — it stays a
`std::time::Duration`. Unlike coordinates (a real range invariant) or
binning (a structured pair), a duration has no invariant `Duration`
doesn't already carry; the only candidate rule, "non-zero," is a *goal*
constraint already enforced by `rp_targets::validate_goals`, not a
property of the value. A newtype would wrap a single std type for no gain
and tax every arithmetic site with an `.as_duration()`.

What keeps it from drifting is **one encoding, not a type**: `humantime`
everywhere (`#[serde(with = "humantime_serde")]` on the field). The
filename-token form (`humantime` with spaces stripped, so the path
component stays space-free and regex-clean) lives with the naming engine
in `rp` — its only consumer. So exposure touches this crate **nowhere**.

The value is named **`exposure_duration`** everywhere: the token
`{exposure_duration}`, the `AcquisitionGoal.exposure_duration` field, the
`(filter, binning, exposure_duration)` quota key. Bare "exposure" is
ambiguous (in astrophotography "an exposure" is a *sub-frame* — a count),
and "exposure_time" is ambiguous the other way (the *time of
acquisition*); "exposure_duration" is the one reading that can't be
misread.

## The coordinate fields

The coordinate drift is prevented structurally by the *field type*, not
by more calls to a validator: `rp_targets::Target`,
`rp::planner::decision::PlannerTarget` and `rp_catalog::ResolvedTarget`
each hold `coord: IcrsCoord`.

With no raw-`f64` coordinate field, every construction — `add_target`,
`update_target`, an import, and any later write path — is *forced*
through `IcrsCoord::try_new` by the compiler: `update_target` builds a
changed coordinate through `try_new` and returns its error to the
caller, and assigning a bare `f64` does not type-check. Serde nests the
coordinate as `"coord": { "ra_hours": <f64>, "dec_degrees": <f64> }`
(above), so the redb rows and the MCP JSON both carry one canonical
`coord` object.

## Schema + validate protocol

`rp-vocabulary` is the *enabler* of a plan-data analogue of the setup
pages' schema and validation; it does not own the protocol. The division:

- **This crate provides**: the validated constructors (the single
  validator, returning typed `thiserror` errors) and — behind the
  `schema` feature — the `JsonSchema` derives with constraints encoded
  (`ra_hours` min/max, `Binning`/`FrameType` shapes), so the
  published schema cannot drift from the validator.
- **`rp` provides**: the `get_plan_schema` / `validate_plan` MCP tools for
  targets, goals, and naming patterns, and the mapping from a constructor `Err` to a
  dotted-path `FieldError { path, msg }` (e.g.
  `{ path: "ra_hours", msg: "ra_hours 25 is outside [0, 24)" }`) — the same shape
  a service's config validation returns, so a UI renders the error next to
  the field.
- **Surfaces consume**: Rust linkers (`rp`, and any tool that links the
  crate) get identical validation by construction; non-linking surfaces
  (the ui-htmx BFF, a browser form) render forms from the published
  schema and defer to `rp`'s constructor as the authoritative gate. This
  keeps ui-htmx a thin renderer (the "UI contains zero application
  logic" tenet), as the setup pages render a service's config from its
  schema.

There is no `apply` tool: the write tools (`add_target`,
`update_target`, `set_goals`) are the apply step, and they share their
rules with `validate_plan` rather than restating them
([rp.md § Plan schema and validation](../services/rp.md#plan-schema-and-validation)).

## Naming: the deferred slice

The file-naming template *engine* lives in `rp` — it is session-layer
glue (regex + chrono + `SessionConfig`) with exactly one linker: no other
service renders or parses filenames (planetarium-bridge creates targets
over MCP and never sees one). What *could* move here is the thin
reusable slice a pattern-editor surface would want — a `NamingPattern`
newtype wrapping the token grammar and `validate_pattern` — leaving
`CompiledTemplate` (render and parse) in `rp`. It moves when a real
second consumer (a pattern editor) exists; the boundary is drawn so the
move is clean.

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
  that driver's *own* frame-safe types (the Star Adventurer GTi's
  `Ra`/`Dec`/`MechHa`/… in `services/star-adventurer-gti/src/units.rs`),
  which model mechanical-safety frame distinctions `IcrsCoord` does not —
  and the driver re-validates against its mechanical envelope regardless,
  because a safety-critical device cannot trust upstream validation. The
  mount and rotator drivers link **no** plan-domain crate (the Star
  Adventurer GTi even wraps `erfars` directly rather than share
  `rp-ephemeris`); a driver
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
pointing device ever earns it (the rotator and the Star Adventurer are
two), the coordinate/angle sharing that belongs
*across drivers* is a neutral `rusty-photon-units` foundation their
frame-safe types build on — never `IcrsCoord` reaching down into a driver.

## Feature flags

```toml
[features]
default = []
schema  = ["dep:schemars"]   # JsonSchema derives on every type
```

`schema` is off by default so a consumer that only needs the DTOs +
validation (`rp-targets`, `rp-catalog`, `rp-ephemeris`) does not inherit
`schemars`. `rp` enables it for the schema/validate protocol. This is the
one lever that keeps the store leaf schemars-free while letting `rp`
project the vocabulary onto the wire.

## Dependencies

| Crate | Purpose |
|---|---|
| `serde` | value (de)serialization, validating on input |
| `thiserror` | the constructor error enums (`CoordError`, `BinningParseError`, `FrameTypeParseError`) |
| `derive_more` | `Display` derives (`Binning`, `FrameType`) |
| `schemars` *(optional, `schema`)* | `JsonSchema` for the wire projection |

Each is a workspace dependency; `serde_json` is a dev-dependency, for
the round-trip tests.

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

Each type's round-trip is pinned in-crate, beside the code it covers,
so a change to one side of a render/parse pair fails a test:

- `IcrsCoord` — `try_new` accepts in-range values, boundaries included,
  and rejects each out-of-range bound; `serde` round-trips through the
  two-key `{ra_hours, dec_degrees}` wire and *rejects* an out-of-range
  wire value on deserialize.
- `Binning` — `Display`/`FromStr` round-trip; `FromStr` rejects non-`AxB`
  and non-`u8`; `serde` round-trips as the `"AxB"` string and rejects the
  struct shape.
- `FrameType` — `Display`/`FromStr` round-trip every variant, and
  `FromStr` is case-sensitive; `calibration_slug`; `serde` round-trips
  through the display string.

Tests use `.unwrap()`/`.unwrap_err()` per
[testing.md](../skills/testing.md), which the workspace `clippy.toml`
allows in test scope.

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
  `rp` is the one service that uses these types directly, and the
  naming *engine* has no second consumer on the roadmap. Several crate
  nodes encoding a future the roadmap doesn't contain would add BUILD
  files and lockfile churn for no consumer they uniquely serve. One leaf,
  with the naming slice's boundary drawn for a later split.
- **Consolidate into existing leaves vs a new crate at all.** A pure
  "no new crate" consolidation (put `Binning` in `rp-targets`,
  `IcrsCoord` in `rp-ephemeris`) fixes one drift but cannot give
  `Target` *and* `PlannerTarget` a shared validated coordinate field
  without an inverted `rp-targets → rp-ephemeris` (ERFA) edge — and it
  scatters the plan vocabulary that the multi-surface schema/validate
  protocol wants in one place. The leaf crate is what makes the
  [coordinate-field guarantee](#the-coordinate-fields) and the
  [schema + validate protocol](#schema--validate-protocol)
  both buildable.

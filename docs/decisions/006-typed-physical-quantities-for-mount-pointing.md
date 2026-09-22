# ADR-006: Typed Physical Quantities for the Star Adventurer GTi Pointing Math

## Status

Accepted (2026-05-24). Implemented on the `feature/sag-typed-units` branch
as a single PR (see [Migration plan](#migration-plan)).

Origin: the review thread on the issue #259 tracking-guard work. Wiring
`MountConfig::validate()` (which closed a pre-existing gap where
`FlipPolicy::validate()` was dead code) surfaced the deeper question —
every physical quantity in the driver is a bare `f64`, so units and
reference frames are enforced only by field-name suffixes, doc comments,
and runtime validation, never by the type system. This ADR proposes the
type-level fix and sequences it as a **separate initiative** from the
shipped #259 work, which stays self-contained.

## Context

The Star Adventurer GTi driver does a lot of pointing arithmetic, and
all of it runs on bare primitives:

- `coordinates.rs` exposes 20 public functions, nearly all taking or
  returning bare `f64` "hours" or "degrees" (53 `f64` mentions in the
  file). `mech_ha` alone appears ~196 times across `src/` as a bare
  `f64`.
- Config carries the same shape: `flip_range_hours`,
  `binding_zone_min_hours`/`binding_zone_max_hours`,
  `tracking_guard_margin_hours`, `dec_min_degrees`/`dec_max_degrees`,
  `site_latitude_deg`/`site_longitude_deg` are all `f64`.
- The workspace's **first frame-distinct newtypes** landed concurrently
  in `pa-falcon-rotator/src/units.rs` (#304, 2026-05-24): `MechanicalDegrees`
  vs `SkyDegrees`, bridged by a `SyncOffset`, hand-rolled over `f64` with
  no external units crate. That establishes the convention; this ADR makes
  `star-adventurer-gti` the **second adopter** and extends it to a richer
  set of astronomical frames. There is still **no units crate**
  (`uom`/`dimensioned`) in the workspace, and none is added here.

### The bug class this is meant to prevent

Two things masquerade as "hours" but are different physical frames:

- **Mechanical HA** (`mech_HA`) — the encoder's view of where the polar
  axis points, folded to `[−12, +12)`. This is what the CW exclusion
  zone, the slew-path checks, and the new tracking guard reason about.
- **Celestial HA** (`HA = LST − RA`), plus the related `RA`, `LST`, and
  `Dec` quantities.

Because they're all `f64`, the compiler accepts any mix of them. Today
the code is correct, but the correctness is unchecked. Two adjacent
lines in `coordinates.rs` make the hazard concrete:

```rust
// mechanical_ha_to_ra: LST minus *mechanical* HA  (frame: mechanical)
(lst_hours - mech_ha).rem_euclid(24.0)
// ra_to_mechanical_ha: LST minus RA = celestial HA (frame: celestial)
fold_to_signed((lst_hours - ra_hours).rem_euclid(24.0), 24.0)
```

Both are `f64 - f64`. Swap `mech_ha` and `ra_hours`, or feed a celestial
HA into a function expecting `mech_HA`, and it compiles and runs — and on
this mount a frame error in the wrong place drives the counterweights
toward the tripod. The same hazard exists on the **Dec axis**: the
encoder's mechanical declination (`[−180, +180)`, runs past the pole) and
the celestial declination (`[−90, +90]`) are both bare degree `f64`s,
related by a `sign·(180 − |d|)` through-the-pole reflection on the flipped
side. The flip math (`mech_ha + 12.0`) and the ticks↔hours↔degrees
conversions (`* 24.0 / cpr`, `/ 15.0`) are the same story: unit- and
frame-laden operations expressed as untyped float arithmetic.

This is exactly the failure mode this driver already takes seriously
mechanically (issue #252's zone widening, #259's tracking guard); the
type system can make a whole category of it unrepresentable.

### Goals

1. Make unit errors (hours vs degrees vs ticks) and frame errors
   (mechanical vs celestial HA, mechanical vs celestial Dec, RA-axis vs
   Dec-axis ticks) **compile errors**.
2. Move config validation from a runtime `validate()` call to
   **construct-time / deserialize-time** invariants (parse-don't-validate).
3. Encode the domain conversions (fold-to-`[−12,12)`, `HA→mech` `+12`
   fold, through-the-pole Dec reflection, tick↔angle) as **named,
   type-checked operations** rather than inline float math.

### Non-goals

- Rewriting other services. They use bare `f64` too, but the bug class
  bites hardest in this mount's pointing math; workspace-wide adoption is
  a later, separate question.
- Adopting a general dimensional-analysis framework (`uom`) — see
  Options.
- Changing any observable behaviour. This is a representation change with
  the existing tests (`coordinates::tests`, `mount_device::tests`, the BDD
  suite, ConformU) as the oracle.

## Options Considered

### Option 1 — Status quo + runtime `validate()` (do nothing more)

Keep bare `f64`; rely on the `MountConfig::validate()` just added and on
test coverage. **Rejected** as the end state: it leaves the frame bug
class entirely unguarded (validation checks ranges, not units/frames),
and it's the situation that prompted the question. Retained only as the
fallback if the migration is judged too risky.

### Option 2 — Config-boundary newtypes only ("Scope A")

Newtype just the config fields with validating constructors + serde
`try_from`, unwrapping to `f64` for all math. **Good but insufficient
alone**: it delivers parse-don't-validate for config (a real win, and it
retires `validate()`), but the moment a value enters `coordinates.rs` it
becomes an untyped `f64` again, so the frame bug class survives. Adopted
here as **part of**, not an alternative to, the full change.

### Option 3 — Semantic domain types across the pointing math (CHOSEN)

Introduce a small set of types that carry both unit and frame, threaded
through `coordinates.rs`, `slew.rs`, `inherent.rs`, the tracking guard,
and config. Distinct types for `MechHa` vs `HourAngle` (celestial) vs
`Ra` vs `Lst`, and `MechDec` vs `Dec`, make the frame errors
unrepresentable; `Cpr`, `RaTicks`/`DecTicks` carry units and axis. See
[Decision](#decision). `pa-falcon-rotator` shipped this exact shape for
its (simpler) two-frame problem, which de-risks it.

### Option 4 — A dimensional-analysis crate (`uom`)

`uom` gives compile-time unit checking for SI-style quantities.
**Rejected**: heavyweight generic machinery and serde-integration
friction for what is, here, a handful of bespoke astronomical frames
(`uom` models units, not "mechanical vs celestial hour angle"). The
frame distinction — the part that actually bites us — isn't something
`uom` expresses. It would also be the workspace's first big external
type dependency for marginal benefit.

### Option 5 — Phantom-typed `Angle<Unit, Frame>`

One generic `Angle<U, F>` with `PhantomData` markers for unit and frame.
**Rejected for readability.** It minimises the number of concrete types
but pushes complexity into the type signatures (`Angle<Hours, Mechanical>`
instead of `MechHa`, with the expanded form leaking into compiler
diagnostics), splits the impls between "generic over `F`" and
"one specific `(U,F)`" blocks, and makes per-frame domain methods
(`Lst::hour_angle_of`) awkward. The payoff — writing the uniform
operations (e.g. the fold) once — is small here because the frame set is
tiny and closed and most operations are bespoke cross-frame conversions.
Concrete newtypes (Option 3) read better, which is the priority, and
match the rotator's choice.

## Decision

Adopt **Option 3**: a `units` module of concrete newtypes, no external
units dependency, hand-rolled in the style `pa-falcon-rotator`
established.

### Type design (as built — `src/units.rs`)

Each newtype holds an `f64`/`i32`/`u32` directly (no inner unit wrapper —
the frame type *is* the unit) and is canonical by construction: every
constructor funnels through a fold/normalise helper. Conversions are the
**only** way to cross a frame boundary, and each is a named, total method.

RA axis (hours):

```rust
pub struct Lst(f64);        // local sidereal time, [0, 24)
pub struct Ra(f64);         // right ascension, [0, 24)
pub struct HourAngle(f64);  // celestial HA = LST - RA, [-12, +12)
pub struct MechHa(f64);     // mechanical (encoder) HA, [-12, +12)
pub struct RaTicks(i32);    // RA-axis encoder counts
```

Dec axis (degrees):

```rust
pub struct Dec(f64);        // celestial declination (not clamped; through-pole detectable)
pub struct MechDec(f64);    // mechanical (encoder) dec, [-180, +180)
pub struct DecTicks(i32);   // Dec-axis encoder counts
```

Shared: `pub struct Cpr(u32);` (per-axis counts-per-revolution; `Cpr(0)`
models "parameters not yet populated" and the conversions degrade to a
zero quantity rather than dividing by zero).

The domain conversions become named, type-checked operations — the inline
float math of today turned into methods that only accept the right frames:

```rust
impl Lst       { pub fn hour_angle_of(self, ra: Ra) -> HourAngle; }  // LST - RA, folded
impl HourAngle { pub fn to_mech(self) -> MechHa; }                   // pre-flip (value preserved)
impl MechHa    { pub fn flipped(self) -> MechHa;                     // post-flip mirror (+12 fold)
                 pub fn to_ticks(self, cpr: Cpr) -> RaTicks;
                 pub fn to_ra(self, lst: Lst) -> Ra;                 // pre-flip
                 pub fn to_ra_flipped(self, lst: Lst) -> Ra; }       // post-flip (+12)
impl RaTicks   { pub fn to_mech_ha(self, cpr: Cpr) -> MechHa;
                 pub fn fold_to_canonical_band(self, cpr: Cpr) -> RaTicks; }
impl Dec       { pub fn to_mech(self) -> MechDec;                    // pre-flip
                 pub fn to_mech_flipped(self) -> MechDec; }          // through the pole
impl MechDec   { pub fn to_dec(self) -> Dec;                         // pre-flip
                 pub fn to_dec_flipped(self) -> Dec;                 // through the pole (self-inverse)
                 pub fn to_ticks(self, cpr: Cpr) -> DecTicks; }
impl DecTicks  { pub fn to_mech_dec(self, cpr: Cpr) -> MechDec;
                 pub fn fold_to_canonical_band(self, cpr: Cpr) -> DecTicks; }
```

**Hand-rolled, no `derive_more` arithmetic.** The operations are
cross-frame (distinct `Output` types), require post-op normalisation, and
— critically — same-frame arithmetic (`MechHa + MechHa`, two absolute
angles summed) is *meaningless* and must stay a compile error. A blanket
`#[derive(Add)]` would skip the fold and re-open exactly the bug class the
newtypes exist to close, so hand-rolling the valid conversions is both
safer and (because the only derivable thing, `Into` to replace `value()`,
would bloat the consuming math) no more code. `derive_more` stays at the
`["debug"]` feature throughout — the config newtypes below validate via
serde's `into`/`try_from` with hand-rolled `From` impls, not a
`derive_more` derive.

### Config newtypes subsume `validate()`

Config fields become validating newtypes; serde validates on
deserialize, so an out-of-range value fails at `serde_json::from_str`
with the field name attached — strictly better than the hand-rolled
`MountConfig::validate()` / `FlipPolicy::validate()`, which are retired:

```rust
#[derive(Serialize, Deserialize)]            // From<_>/Into hand-rolled (no derive_more feature)
#[serde(try_from = "f64", into = "f64")]     // JSON stays a bare number
pub struct FlipRangeHours(f64);

impl TryFrom<f64> for FlipRangeHours {
    type Error = String;
    fn try_from(v: f64) -> Result<Self, String> { /* (0, 0.95] */ }
}
```

> **Note (2026-09):** `FlipRangeHours` and its `flip_range_hours` config
> field were removed — the quantity they described turned out to be the
> wrong shape for the mechanism (see the star-adventurer-gti design doc,
> §Flip policy, and issue #1301). The pattern this ADR records is
> unaffected; `TrackingGuardMarginHours` and `MinAltitudeDegrees` are
> live examples of it. The decision text below is left as it was written.

The scalar fields (`FlipRangeHours`, `TrackingGuardMarginHours`) keep a
bare-number JSON form. The CW zone becomes one `CwExclusionZone`
(`Active(ActiveZone) | Disabled`), modelled as `Option<ActiveZone>` on
the wire — an `{ min_hours, max_hours }` object, or `null` for disabled —
so the "disabled" state is explicit rather than the old `min ≥ max`
convention; `ActiveZone` validates `-12 ≤ min < max ≤ 12`. `DecLimits`
is the analogous `{ min_degrees, max_degrees }` type. (`DecLimits` was
later replaced by the bare-number `MinAltitudeDegrees` when the
altitude floor superseded the rectangular Dec envelope, 2026-07-01 —
same newtype pattern.) Defaults live on
the types (`Default` impls), so the fields use bare `#[serde(default)]`.

As-built notes: because there are no operators yet, the config JSON
**schema changed** to this cleaner nested form rather than preserving the
old flat `binding_zone_min/max_hours` / `dec_min/max_degrees` keys (no
backwards-compatibility constraint). `derive_more` stays at `["debug"]`;
the handful of `From` impls are hand-rolled rather than growing its
feature set.

### No external units dependency

Use `derive_more` only (and only its `["debug"]` feature plus `Into` on
the config newtypes). Reject `uom` (Option 4).

## Migration plan

The whole change lands as **one PR**, committed at green checkpoints (the
ADR's earlier draft proposed four reviewable phases with transitional
`f64` shims that were then deleted; landing in one PR makes the
shim-then-remove ceremony unnecessary — signatures flip straight to
typed). `coordinates.rs` is hardware-validated (Phase 4/6, lat 32.7°N)
and safety-critical, so the existing tests are the oracle at every
checkpoint and the suite stays green throughout. Checkpoints:

1. **`src/units.rs` + tests** — the types with exhaustive unit + property
   tests (tick↔angle round-trips, `fold` idempotence, `HA→mech→HA` and
   through-pole round-trips). Pure addition; nothing calls them yet.
   *(landed: commit `b00364a`.)*
2. **`coordinates.rs` migrated** — public signatures flipped to typed; the
   `coordinates::tests` call sites updated to construct/extract types with
   their assertions (expected values) unchanged, so they remain the
   regression net.
3. **Consumers migrated** — `slew.rs`, `tracking_guard.rs`, `inherent.rs`,
   `telescope.rs`, `watchers.rs`, `device.rs`, and `mount_device::tests`
   construct types at the `f64` boundaries and unwrap results back to
   `f64` where they meet the ASCOM trait API (`ascom-alpaca`'s `f64`
   RA/Dec), the Sky-Watcher wire codec, and serde — the three boundaries
   that stay `f64` by necessity, each documented.
4. **Config Scope A** — the validating newtypes; `MountConfig::validate()`
   / `FlipPolicy::validate()` retired in favour of construct-time
   validation; the `validate()` test sites migrated to deserialize/
   construct assertions.

(2)–(4) may be grouped into fewer commits where flipping a signature
forces its callers to change atomically. BDD scenarios and ConformU are
unchanged and serve as end-to-end regression checks across all checkpoints.

## Consequences

- **Eliminates the frame bug class** in pointing math (RA *and* Dec axes)
  and makes config invalid-states unrepresentable — the two things this
  initiative exists for.
- **Second adopter, shared convention.** `pa-falcon-rotator` established
  the hand-rolled-newtype convention; this service adopts and extends it.
  The convention (newtype-per-quantity, parse-don't-validate config) is
  recorded in `docs/skills/development-workflow.md`. The `units` types
  themselves stay service-local; a future ADR can promote a shared
  `rusty-photon-units` crate if a third pointing device earns it.
- **Friction at boundaries.** Every `f64` site gains a wrap/unwrap or a
  method call, and three boundaries stay `f64` by necessity — serde
  input, the Sky-Watcher wire codec, and the `ascom-alpaca` trait API
  (RA in hours, Dec in degrees). These are localised and documented;
  the interior is typed.
- **Migration risk is the main cost** — it rewrites validated pointing
  code. Mitigated by the existing test oracle and keeping each checkpoint
  a green commit.
- **No new external dependency.** `derive_more` stays at `["debug"]` (plus
  its `Into` derive on config newtypes); no `uom`.
- **`#259` is unaffected.** The shipped tracking guard kept working; its
  `validate()` was swapped for typed construction without behaviour
  change.

## Resolved questions

1. **Frame granularity** — concrete `MechHa`/`HourAngle`/`Ra`/`Lst`,
   `MechDec`/`Dec`, `RaTicks`/`DecTicks` (this ADR), **not** a
   phantom-typed `Angle<Unit, Frame>` (Option 5). Decided on readability;
   the rotator's concrete choice is the precedent. The RA/Dec axes are
   treated symmetrically, and RA-axis vs Dec-axis ticks are distinct types.
2. **Scope A** — the config newtypes land in the same single PR (not a
   separate earlier phase), and fully retire `validate()`.
3. **Workspace-wide adoption** — out of scope here; the *convention* is
   documented now, the *types* stay service-local, and the rotator +
   this service are the two precedents a future promotion would build on.

## References

- Issue #259 review thread — the validation work that surfaced this.
- `services/pa-falcon-rotator/src/units.rs` — the workspace's first
  frame-distinct newtypes (#304); the style this ADR matches.
- `services/star-adventurer-gti/src/units.rs` — the types this ADR adds.
- `services/star-adventurer-gti/src/coordinates.rs` — the math being
  typed (the `lst - mech_ha` vs `lst - ra` frame hazard).
- `services/star-adventurer-gti/src/config.rs` — `MountConfig::validate`
  / `FlipPolicy::validate` (the runtime checks construct-time validation
  subsumes).
- `docs/skills/development-workflow.md` — the newtype / parse-don't-validate
  convention.
- `docs/services/star-adventurer-gti.md` §"Safety envelope" /
  §"Tracking-time safety guard" — why frame correctness is a hardware
  concern on this mount.
- `docs/decisions/004-testing-strategy-for-http-client-error-paths.md` —
  prior workspace-convention ADR; format reference.

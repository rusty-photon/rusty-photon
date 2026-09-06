# Star Adventurer GTi: meridian-flip support + envelope refinements

**Status: COMPLETE (archived 2026-09-05).** Every phase shipped to
`main`: 1.1's envelope defaults (#230, superseded two days later by
#252's asymmetric `cw_exclusion_zone` and its path-crossing checks),
1.2 dropped as superseded by 1.1's hour-margin framing, 2's
meridian-flip support (#244, hardware-validated 2026-05-16 at lat
32.7°N), 2.5's driver-planned auto-flip during tracking (issue #259
Part 2, riding the tracking watcher #303 added for Part 1), and 3's
altitude-based safety floor (#421), which removed the rectangular
`dec_limits` envelope. The driver behaviour is documented in
[`star-adventurer-gti.md`](../../services/star-adventurer-gti.md).
Deferred: real-hardware validation of an *unattended* auto-flip session,
under the same operator-supervised protocol as §2.8 —
`flip_policy.enabled` and `flip_policy.auto_flip_during_tracking` both
default `false` until an operator has replayed the validation. The
per-phase record below is kept as written.

## Status

- **Phase 1.1 — envelope defaults (±6.95 h, hour-margin framing): LANDED**
  in commit `b8e83bc` (2026-05-15) on
  `feature/star-adventurer-gti-envelope-refinements`, merged as PR #230.
  Default `MountConfig::ra_min_hours` / `ra_max_hours` moved from `±6.0`
  to `±6.95` — 3 arcmin inside the GTi's hardware-verified `±6.99 h`
  mechanical limit and INDI eqmod's baked-in `±7 h` envelope.
  **Superseded 2026-05-17 by PR #252:** after an OTA–tripod contact
  during a `SetSideOfPier` from Park 3, the RA band was replaced by the
  asymmetric `MountConfig::cw_exclusion_zone` (default
  `(0.95, 11.05)` mech-HA hours, JSON `null` to disable) plus
  path-crossing checks; the `ra_min_hours` / `ra_max_hours` fields no
  longer exist. A tracking-time safety guard
  (`tracking_guard_margin_hours`, issue #259 Part 1) later extended the
  zone's enforcement to tracking drift.
- **Phase 1.2 — tick-slack at the boundary: DROPPED.** Superseded by
  the hour-margin framing in 1.1: with the configured envelope sitting
  3 arcmin inside the mechanical limit, ASCOM `SlewToCoordinates`
  LST-roundtrip drift no longer pushes boundary targets over the
  cliff. No CPR-dependent comparator constant needed.
- **Phase 2 — meridian-flip support: hardware-validated 2026-05-16**
  on branch `worktree-meridian-flip-phase`, merged as PR #244 (titled
  "Phase 6" in the service's chronological PR sequence; "Phase 2" is this
  plan's own local numbering). Scope landed in PR #244:
  §§2.1, 2.2, 2.3, 2.4, 2.6, 2.7 — coordinate helpers, per-pier-side
  envelopes, asymmetric counterweight binding-zone envelope,
  binding-zone-path-aware through-wrap RA routing,
  visible-pole Dec routing, `SetSideOfPier`, the `enabled`
  + `flip_range_hours` config fields, and BDD coverage.
  §2.5 (auto-flip during tracking) and the
  `auto_flip_during_tracking` / `auto_flip_at_meridian_offset_hours`
  config fields were deferred to a follow-up — hosts (NINA / SGP /
  rp) own flip timing in MVP; explicit `SetSideOfPier` calls drive
  flips. §2.8 hardware validation completed at lat 32.7°N: end-to-end
  AP Park 1–5 traversal including the through-wrap saddle-east flip
  and its flip-back. A flip-back regression (sign-blind heuristic in
  `flip_slew_ra_delta`) was caught mid-session and fixed in commit
  `072cc72`. `flip_policy.enabled` still defaults `false` — operators
  opt in once they've replayed the validation locally.
- **Phase 2.5 — auto-flip during tracking: IMPLEMENTED (2026-07-12,
  branch `worktree-issue-223`; Part 2 of issue #259).** The
  `flip_policy.auto_flip_during_tracking` (default `false`) and
  `auto_flip_at_meridian_offset_hours` (default `0.0`, cross-validated
  to `±flip_range_hours` at config load) fields landed, wired into the
  per-connection tracking watcher that already runs the Part 1
  stop-only safety guard (issue #259 Part 1, PR #303). While tracking
  on the natural pier side, the watcher triggers the same through-wrap
  flip slew `SetSideOfPier` issues once the live encoder `mech_HA`
  reaches the offset; the completion watcher re-engages tracking on
  the new side. One attempt per meridian crossing — a failed attempt
  warns and leaves the stop-only guard as the fallback, and the guard
  takes precedence when `mech_HA` is already inside the guarded band.
  Unlike §2.5's original sketch there is no separate state machine:
  the existing slew-completion watcher already owned stop → slew →
  re-engage, so auto-flip is one trigger decision per watcher tick.
  Real-hardware validation of an unattended auto-flip session is
  pending (same operator-supervised protocol as §2.8).
- **Phase 3 — altitude-based safety floor: IMPLEMENTED (2026-07-01,
  branch `worktree-issue223`; PR #421).** Replaces the rectangular Dec
  envelope (the former `MountConfig::dec_limits: DecLimits` struct,
  removed by this phase — the `dec_min_degrees` / `dec_max_degrees`
  field names in §§3.3–3.5 below predate even that struct) with a single
  `min_altitude_degrees` floor computed from HA + dec + site latitude.
  The rectangular envelope doesn't follow the tilted local-horizon
  circle on the celestial sphere; the altitude floor does. Independent
  of Phase 2 (no interaction; can land in either order).

  **Current-code refresh (2026-07-01 audit before implementation):**
  the sections below were written against the pre-PR-#252 code and are
  stale in three ways. (1) §3.3's "keep the existing RA mech-HA check"
  now means keeping the `cw_exclusion_zone` destination + path checks,
  not an `ra_min/ra_max` band. (2) `check_within_safe_envelope` already
  has the signature Phase 3 needs —
  `(ra_hours, dec_degrees, lst_hours, target_is_flipped)` — so the
  §3.3 call-site change is moot. (3) Config validation moved to
  parse-don't-validate newtypes (`FlipRangeHours`, `CwExclusionZone`,
  `DecLimits`, …), so `min_altitude_degrees` lands as a validating
  `MinAltitudeDegrees` newtype, not a bare `f64`. One addition to
  §3.6/§3.7: the BDD suite cannot pin LST (no clock injection), so the
  BDD default config neutralises the floor at `-90°` (mirroring how it
  disables `cw_exclusion_zone`), the altitude scenarios configure the
  floor explicitly and address targets by *hour angle* (steps compute
  `RA = LST − HA` at test time), and exact-boundary cases live in unit
  tests where HA is passed directly. On §3.5: the config structs do
  not use `deny_unknown_fields` (schema-driven `config.apply` tooling
  round-trips full JSON across versions), so "strict-from-start" is
  moot — a stale `dec_limits` key in an existing config file is
  silently ignored rather than rejected at load.

  **As-built notes (2026-07-01):**

  - §3.2's `target_altitude_degrees` helper was **not added** — the
    identical spherical-astronomy formula already existed as
    `coordinates::ra_dec_to_alt_az` (it backs the ASCOM `Altitude` /
    `Azimuth` reads), and the envelope check reuses it. The §3.1
    worked-example table is pinned by a unit test against that
    function instead.
  - The `dec_limits` removal was re-examined before landing: the
    counter-argument was that `dec_limits` might be a *mechanical*
    restriction (distinct from the floor's image-quality purpose) and
    should stay. Resolved to remove it per §3.4 after review: the
    field was enforced against *celestial* Dec (identical for both
    pier sides), so it never expressed a mechanical Dec-axis bound —
    the mount's mechanical protection is `cw_exclusion_zone`, and the
    celestial `[-90, +90]` clamp lives in `validate_coordinates`. If
    a genuine mechanical Dec-encoder constraint ever surfaces
    (cabling, camera-vs-tripod clearance at past-pole rotations), it
    should be designed as a *mechanical-encoder-space* limit, not a
    celestial-Dec rectangle.
  - The floor gates slew, sync, and `DestinationSideOfPier`
    validation (everything routed through
    `check_within_safe_envelope`); `Park` stays exempt
    (privileged-park pattern, plan §3.8 option (a)).
  - `MinAltitudeDegrees` accepts `[-90, +90]`; `-90` never rejects,
    which is what the BDD default test config and the
    envelope-isolation unit-test builders use to neutralise the gate.

This plan covers related items that surfaced while landing
issue #202 (Dec-encoder `SideOfPier` + `DestinationSideOfPier`).

## Motivation

Issue #202 switched `SideOfPier` from the RA mech-HA split to the
canonical INDI eqmod Dec-encoder convention and implemented
`DestinationSideOfPier`. The ConformU baseline moved from 9 issues to 7
— same compliance status, different breakdown. Three of the new issues
are ConformU flagging the difference between flip-aware GEM behaviour
and our non-flipping mount:

- `SideofPier ×1` — "`pierWest` is returned when the mount is observing
  at HA 0 to +6"
- `DestinationSideofPier ×1` — same, prediction side
- `DestinationSideOfPier ×1` — "Same value on both sides of the
  meridian"

These are not bugs against the ASCOM spec — the spec says SideOfPier
reports the OTA's mechanical pier position, not the target's sky
position, and a mount that doesn't flip correctly stays on the same
side. But they go away once the driver actually plans flips.

A hardware verification session against a live GTi (PR #220 review)
surfaced the envelope refinement that became Phase 1.1: the GTi
mechanically reaches ±6.99 h cleanly (verified on hardware
2026-05-13), past the previous `±6 h` default.

---

## Phase 1.1 — Envelope defaults (LANDED)

Bumped `default_ra_min_hours()` / `default_ra_max_hours()` in
`services/star-adventurer-gti/src/config.rs` from `±6.0` to `±6.95`,
with the doc-comment and design-doc text rewritten to make clear the
configured envelope sits **inside** the mechanical limit, not at it.

The buffer (3 arcmin / 0.05 h) covers two needs at once:

1. The ASCOM `SlewToCoordinates(ra, dec)` round-trip means the driver
   re-reads LST a few tens of ms after the client computed the target,
   so a target quantised exactly to a mechanical-limit-equivalent
   would drift past it. The hour-margin absorbs that drift without
   needing a tick-precision slack term in the comparator.
2. The deferred Phase 2 meridian-flip planner will need headroom
   between the configured envelope and the mechanical stops to plan
   multi-stage flip slews — see Phase 2 below.

Files changed in commit `b8e83bc`:

- `services/star-adventurer-gti/src/config.rs` — defaults + doc-comment
- `services/star-adventurer-gti/src/mount_device.rs` — two
  `fast_settle_*` comments
- `docs/services/star-adventurer-gti.md` — §"Mechanical safety
  envelope" + ConformU expected-issues references

---

## Phase 2 — Meridian-flip support

### 2.0 Geometric finding: the GTi *can* flip near the meridian

Walking through the geometry of a meridian flip in detail
([conversation 2026-05-14 → 2026-05-15][flip-discussion]) revealed
that the GTi is mechanically capable of performing a flip for targets
near the meridian, provided the slew planner picks the right
encoder-routing direction. The flip is no longer gated on acquiring a
flip-capable Sky-Watcher mount — though hardware verification on the
GTi is still required before landing the production code.

[flip-discussion]: # "Conversation between user and Claude Code, 2026-05-14 to 2026-05-15."

#### Geometric primer (codebase convention, LAT 45°N example)

For a polar-aligned GEM, the saddle (one end of the Dec axis) and
counterweight (the other end) lie diametrically opposite on the
**celestial equator** — they are perpendicular to the polar axis, so
both project onto the equator regardless of the dec encoder value. Their
local altitudes are mirror images:

```
saddle_alt = -CW_alt        (always)
```

For LAT 45°N, the celestial equator traces a tilted great circle through
the local sky:

| Position on equator | local alt | local az |
|---|---|---|
| east horizon | 0° | east (90°) |
| south meridian (HA = 0) | +45° | south (180°) |
| west horizon | 0° | west (270°) |
| north anti-meridian (HA = ±12) | -45° | north (0°) |

The codebase uses `saddle_HA = mech_HA + 6` on this equator (Convention
1: dec axis east-west horizontal at `mech_HA = 0`, saddle at the WEST
end, CW at the EAST end). Equivalently:

| `mech_HA` | Saddle alt | **CW alt** | CW position |
|---|---|---|---|
| -12 (= +12 wrap) | 0° | 0° | west horizon |
| -6 | +45° (south meridian) | **-45°** | north anti-meridian |
| 0 (home) | 0° (west horizon) | 0° (east horizon) | east horizon |
| +6 | -45° (north anti-meridian) | **+45°** (south meridian) | counterweight-up MAX |
| +9 | -30° | **+30°** WSW | **Phase 4 binding zone** |
| +12 (= -12 wrap) | 0° | 0° | west horizon |

The "counterweight-up region" — where Phase 4 saw the motor stall
against a hard stop — is on the **positive `mech_HA`** side
(`mech_HA ∈ (0, +12)`), with the CW above the local horizon. The
symmetric **negative `mech_HA`** side keeps the CW below the horizon
and stays clear of the binding region.

#### Flip procedure (LAT 45°N, target HA = −0.5, dec = +45° example)

The target is near zenith, just east of the meridian. A meridian
flip transitions the mount from normal pointing to flipped pointing
while keeping the OTA on the same celestial target.

**Pre-flip state:**

- `mech_HA = -0.5`, `dec_encoder = +45°`
- Saddle: just past west horizon (alt +5.3°, mostly west, slightly south-up)
- CW: just below east horizon (alt -5.3°, mostly east, slightly north-down)
- OTA tube: pointing at target near zenith east (alt +84.5°)

**Step 1 — slew to home (intermediate park):**

- `mech_HA = 0`, `dec_encoder = +90°`
- Saddle at west horizon (alt 0°)
- CW at east horizon (alt 0°)
- OTA tube parallel to polar axis, pointing at Polaris / NCP

This is the user-friendly "counterweights perpendicular to horizon,
OTA pointing north" intermediate position. (At LAT 45°N the CW is at
east horizon, alt 0°, not strictly straight down — the geometric
generalisation across latitudes is "CW at the position where the
local horizon crosses the celestial equator", with the OTA tube
re-aimed along the polar axis at the celestial pole.)

**Step 2 — slew to flipped pointing through the negative-`mech_HA` direction:**

- Target `mech_HA = +11.5` (equivalently `-12.5` via the encoder
  wrap at ±12)
- Target `dec_encoder = +135°` (past the celestial pole)

The slew planner must pick the **negative-direction** route, not
shortest-encoder-path. Walking the route:

| `mech_HA` | Saddle alt | **CW alt** | Notes |
|---|---|---|---|
| 0 (home) | 0° (W horizon) | 0° (E horizon) | start |
| -3 | +22° | **-22°** | CW going below horizon |
| -6 | +45° (south meridian) | **-45°** (north anti-meridian) | CW at deepest below horizon |
| -9 | +30° | **-30°** WNW | CW still below horizon |
| -12 (= +12 wrap) | 0° (E horizon) | 0° (W horizon) | encoder wrap point |
| +11.5 | -5.3° | **+5.3°** | post-flip end state |

Throughout the 12 h slew, the CW stays **at or below the local
horizon**, only rising above the horizon by **at most 0.5 h
(alt +5.3°)** in the final fraction of a degree after the encoder
wrap. The slew never enters the counterweight-up region and so
never approaches the Phase 4 binding zone.

The dec axis traverses through the celestial pole simultaneously
(`dec_encoder` going from +90° at home to +135° at the flipped state),
so the OTA tube ends up on the opposite end of the Dec axis from where
it started.

**Post-flip state:**

- `mech_HA = +11.5`, `dec_encoder = +135°`
- Saddle: just below east horizon (alt -5.3°)
- CW: just above west horizon (alt +5.3°) — "counterweights west"
- OTA tube: pointing at target near zenith east (same celestial target as pre-flip)
- Image is rotated 180° in the camera — standard meridian-flip image
  rotation; handled by camera-control software or post-processing.

#### Why the routing works

The mechanical binding the GTi exhibited in Phase 4 is in the
**counterweight-up region** — when the counterweight extends above the
local horizon level on the same side as the pier head, the arm geometry
contacts the pier or the cable wrap binds. This region corresponds to
`mech_HA ∈ (0, +12)` in the codebase convention (CW positive altitude),
peaking at `mech_HA = +6` (CW at alt +45° south).

The symmetric `mech_HA ∈ (-12, 0)` side keeps the counterweight below
the horizon throughout — the CW sweeps from east horizon through
north-down (alt -45°) to west horizon, never approaching the pier from
above. The dec axis rotates 180° around the polar axis, but the path
of that rotation goes "under" the mount rather than "over" it.

The "image flipped upside down" is the price: in the post-flip state,
the OTA tube has rotated 180° around its own optical axis (Dec encoder
past pole = OTA tube on the other end of the Dec axis), so the camera
sees the field rotated 180°. This is the same as a standard meridian
flip on any GEM.

#### Operator's short version (NH)

When the target approaches the meridian, the mount:

1. Moves to the "counterweights down, perpendicular to horizon"
   intermediate position with the OTA pointing north (at the celestial
   pole / Polaris).
2. Continues rotating the RA axis in the same direction (through the
   counterweight-down half of the encoder range, then through the
   encoder wrap at ±12) to land on the counterweights-west position.
3. Rotates the dec axis through the pole simultaneously, ending with
   the OTA pointing back at the target.

This routing never takes the counterweight through the
counterweight-up-against-pier region, so cable tangles and pier
contact are avoided.

### 2.1 Coordinate-math layer

New helpers in `services/star-adventurer-gti/src/coordinates.rs`:

- `target_encoder_normal(ra, dec, lst, cpr_ra, cpr_dec) -> (i32, i32)`
  — `(mech_HA, dec_celestial)` mapped to encoder ticks; the current
  behaviour, extracted.
- `target_encoder_flipped(ra, dec, lst, cpr_ra, cpr_dec) -> (i32, i32)`
  — `mech_HA_flipped = mech_HA_normal + 12 h` (mod 24, signed),
  `dec_flipped = sign(dec) · (180° − |dec|)` (signed magnitude past
  the pole).
- `select_pier_side_for_target(target_ra, lst, current_pier_side, policy) -> PierSide`
  — flip-policy decision, taking the current state and the target.

### 2.2 Per-pier-side safe envelopes

The existing single envelope (`MountConfig::{ra_min_hours,
ra_max_hours, dec_min_degrees, dec_max_degrees}`) covers pre-flip
(pierWest) operation. Phase 2 needs the symmetric envelope for the
post-flip (pierEast) state:

- **Pre-flip (pierWest) safe zone:** RA `mech_HA ∈ [-6.95, +6.95]`,
  Dec encoder `∈ [-90°, +90°]` (the current envelope).
- **Post-flip (pierEast) safe zone:** RA `mech_HA ∈ [-12, -11.05]` ∪
  `[+11.05, +12]` (mirror across the encoder wrap at ±12, with the
  same 3-arcmin margin as the pre-flip zone), Dec encoder past the
  pole `∈ [+90°, +180°]` ∪ `[-180°, -90°]`.

The post-flip RA bounds are derived by **symmetry with the pre-flip
bounds**: Phase 1 verified that the GTi reaches `mech_HA = ±6.99 h`
cleanly past counterweight-horizontal at `mech_HA = ±6`. The mirror
region near the encoder wrap (`mech_HA ≈ ±12`) puts the counterweight
in the corresponding "just-past-horizontal" position on the opposite
side; the symmetry argument is plausible but the post-flip bounds
remain hardware-unverified until §2.8 happens.

### 2.3 Slew planner with through-wrap routing

Modify `slew_to_coordinates_async` in
`services/star-adventurer-gti/src/mount_device.rs`:

1. Determine current pier side from encoder state.
2. Pick target pier side via flip policy (§2.1 helpers).
3. Compute target `(ra_ticks, dec_ticks)` for the chosen side.
4. Validate against the relevant per-side envelope (§2.2).
5. If pier side is changing, this is a **flip slew**. Route the RA
   axis through the **counterweight-below-horizon half** of the
   encoder range (the negative-`mech_HA` direction for the LAT-45-N
   example above; the generalisation across latitudes is "the
   direction that keeps `CW_altitude < 0` throughout the slew").
   This is a single longer slew, not a multi-stage motion; the
   "through home" feel comes naturally from the route passing
   through `mech_HA = 0` on its way to `mech_HA ≈ -12` and then
   wrapping to the flipped target.
6. Issue wire sequence on both axes simultaneously (RA through the
   wrap, dec through the pole).

### 2.4 ASCOM `SetSideOfPier`

Currently returns `NOT_IMPLEMENTED`. With flip support:

- `CanSetPierSide = true`.
- `SetSideOfPier(side)` forces a flip if current side ≠ requested.
- `DestinationSideOfPier(ra, dec)` uses the flip policy (§2.1) rather
  than always returning the same side.

### 2.5 Tracking auto-flip

Optional (opt-in via config). State machine:

- `Tracking` → "approaching pre-flip envelope edge" → "flip pending" →
  "executing flip" → "tracking on new side".
- Default **off**. Hosts like NINA / SGP control flip timing
  themselves and a mid-exposure auto-flip is a footgun for
  astrophotography.

### 2.6 Configuration

A new `flip_policy` block on `MountConfig`:

```json
"flip_policy": {
  "enabled": false,
  "flip_range_hours": 0.5,
  "auto_flip_during_tracking": false,
  "auto_flip_at_meridian_offset_hours": 0.0
}
```

Field semantics:

- **`enabled: bool`** (default `false`) — master switch. Until set
  `true`, `CanSetPierSide` reports `false`, `SetSideOfPier(East)`
  returns `NOT_IMPLEMENTED`, and the slew planner ignores flipped
  pointing entirely (behaviour identical to pre-Phase-2). Stays
  `false` in the shipped default until the first real flip on a GTi
  has been hardware-validated (§2.8); operators opt in per-mount.

- **`flip_range_hours: f64`** (default `0.5`) — half-width of the
  target-HA window around the meridian where the flipped state is
  mechanically reachable on the GTi. For target HA outside
  `[−flip_range_hours, +flip_range_hours]` the slew planner refuses
  flipped pointing and uses normal pointing only. Valid range
  `(0, 0.95]`; the upper bound is the verified safe headroom past
  counterweight-horizontal on the pre-flip side, and a `flip_range`
  larger than that would push the post-flip `mech_HA` into the
  unverified binding zone on the wrap side.

- **`auto_flip_during_tracking: bool`** (default `false`) — if `true`,
  the driver initiates a meridian flip on its own when continuous
  tracking crosses the configured trigger. Default `false` because
  hosts like NINA / SGP own flip timing themselves via `SetSideOfPier`
  ASCOM calls, and an unexpected mid-exposure auto-flip breaks
  astrophotography frames. Operators running fully unattended sessions
  can opt in.

- **`auto_flip_at_meridian_offset_hours: f64`** (default `0.0`) — only
  used when `auto_flip_during_tracking = true`. Target HA at which the
  auto-flip fires:
  - `0.0` → flip exactly at meridian crossing
  - `+0.3` → wait until target is 0.3 h past meridian, then flip
    (matches NINA's "delay meridian flip by N minutes" semantics)
  - Must be inside `[−flip_range_hours, +flip_range_hours]`.

  A small positive offset is the common astrophotography preference —
  it lets the in-progress sub finish before the flip kicks in.

Deliberately omitted, with rationale:

- **`flipped_ra_min_hours` / `flipped_ra_max_hours`** — derivable from
  `flip_range_hours` (post-flip `mech_HA = target_HA − 12`, so the
  post-flip safe band is `[−12, −12 + flip_range_hours] ∪ [12 −
  flip_range_hours, +12]`). Adding redundant fields invites drift.
- **`flip_slew_direction` / `flip_through_wrap`** — the slew-routing
  choice (positive direction vs through-wrap) is mount-specific
  implementation, not user policy. On the GTi the planner always picks
  through-wrap; on flip-capable mounts (EQ6-R Pro, AZ-EQ6, HEQ5 Pro)
  it would pick shortest-path. Lives in code.
- **`pier_side_override` per-target / per-RA-range** — useful for
  cable-management on big mounts with asymmetric routing; over-
  engineering for the GTi MVP. Defer to a follow-up if requested.
- **`flip_safety_margin_hours`** — fold into the `flip_range_hours`
  default rather than expose two redundant knobs.

### 2.7 BDD scenarios

New feature files / scenarios:

- `SetSideOfPier` forcing a flip.
- Slew planning across each `(current_side, target_side)` pair.
- The through-wrap slew routing — assert the RA encoder traverses
  through the negative-direction half of the encoder range and the
  CW altitude stays below horizon throughout.
- `DestinationSideOfPier` returning each side based on policy and
  target HA.
- Flip during tracking (if 2.5 is enabled).
- Abort during a flip.
- Park from flipped state.
- Sync while flipped.

### 2.8 Hardware validation on the GTi

The negative-direction half of the encoder is mechanically symmetric
to the positive-direction half that Phase 1 already verified. Normal
tracking westward through the safe envelope sweeps the counterweight
through the **upper arc** (CW altitudes 0° → +45° → +43° as `mech_HA`
goes 0 → +6 → +6.95); the mirror motion eastward sweeps the
counterweight through the **lower arc** (CW altitudes 0° → -45° →
-43°). Both arcs are exercised during every observing session and
neither has been reported to bind in real use.

The through-wrap slew extends the lower-arc traversal past the safe
envelope edge into `mech_HA ∈ (-12, -6.95)`. The mirror of Phase 4's
`mech_HA = +9` binding (CW at +30° south-west, alt-positive) is at
`mech_HA = -9` (CW at -30° west-north, alt-negative) — same arm
angle relative to the pier, just on the opposite side of the meridian
and below horizon. By the mount's symmetric structure around the
polar axis, if one side binds, the other side binds too; if one side
clears, the other clears. **Phase 1's success on the positive side at
±6.99 h carries to the negative side, and the through-wrap routing
becomes verifiable by the first real flip on the GTi rather than by a
dedicated pre-implementation test.**

The one asymmetric failure mode worth flagging is **cable wrap** —
mount cabling can route one way around the polar axis such that
clockwise vs counterclockwise rotation aren't mechanically equivalent.
This will surface loudly the first time a real flip executes on
hardware, and is detectable / non-destructive.

Validation plan, integrated into normal Phase 2 hardware-bringup:

1. **First `SetSideOfPier(East)` on a real GTi.** The mount executes
   the through-wrap slew end-to-end. If cable wrap, dec-past-pole
   gearbox, or any other asymmetric failure mode triggers, this run
   exposes it without destroying anything (the motor stalls audibly
   as Phase 4 documented).
2. **Pickup-loop / watcher behaviour across the encoder wrap.** The
   RA encoder traverses `±12` mid-slew. Verify the slew watcher and
   pickup loop handle the wrap point in their delta computations
   (this is firmware-level behaviour and should already be correct,
   but worth a wire-trace pass).

#### Outcome (2026-05-16, lat 32.7°N)

Both validation items passed. Cable wrap was not exercised destructively
(the user kept hand-stop authority over the mount throughout). The
through-wrap saddle-east flip (`SetSideOfPier(East)` from
`(mech_HA = 0, dec = -57.3°)` pierWest staging) issued RA −1.81 M ticks
CCW (−180° polar-axis) + Dec +2.38 M ticks CW (+294° through wrap past
NCP), landing cleanly at Park 4 N (saddle east, OTA south horizon
level). No motor stall, no audible binding, no transport-layer fallout.

The flip-back from saddle-east (Park 4 N → Park 5 N at
`(mech_HA = ±12, dec = +57.3°)`) exposed a sign-blind heuristic in
`flip_slew_ra_delta`. The function's old rule —
`|current_ticks| > cpr_ra/4 ⇒ "safe direction is positive"` — was
designed for the `+cpr_ra/2` wrap (where positive direction wraps into
the safe negative half) and inverted incorrectly at `-cpr_ra/2`. For
the Park 5 slew, current was `-1,810,272` (just east of the
saddle-east wrap on the negative side), canonical short delta was
`-3,798` ticks CCW (a ~0.03° polar-axis nudge that physically just
crosses the `-12 ↔ +12` wrap, staying in the safe arc). The old rule
flipped that to `+cpr_ra - 3,798 = +3,625,002` ticks CW, a 359.6°
polar-axis revolution that swept `mech_HA` through the binding zone
and slammed the CW shaft into the pier. The operator powered the
mount off mid-sweep.

Fix landed in commit `072cc72` — the routing now checks whether the
canonical short delta's mech_HA sweep actually crosses
`(zone_min, zone_max)`, using the configured binding zone (checked
modulo 24 h at `k ∈ {-1, 0, +1}` to handle the wrap). Re-run of the
same Park 5 slew issued RA −3,631 ticks CCW (the canonical safe step)
and the mount landed at Park 5 N visually confirmed.

A structurally analogous sign-blindness existed in `flip_slew_dec_delta`
— the dec routing assumed "safe direction in north for the post-flip half
is negative" but inverted at the `-cpr_dec/2` side of the wrap. This
session's Park 4 → Park 5 dec was on the `+cpr_dec/2` side where the old
heuristic happened to be correct, so the dec bug wasn't exercised live. It
was corrected in the same meridian-flip work rather than deferred: both
`flip_slew_ra_delta` and `flip_slew_dec_delta` now route via the path-aware
`canonical_path_crosses_pole` / binding-zone checks instead of the
`|current| ≷ cpr/4` sign heuristic (see `mount_device/slew.rs`).

---

## Phase 3 — Altitude-based safety floor

### 3.0 Motivation

The current Dec safety envelope is rectangular in encoder space
(`MountConfig::dec_min_degrees` / `dec_max_degrees`, defaults
`[-90°, +90°]`). The default is essentially a no-op (full celestial
Dec range), and tightening it doesn't capture what operators actually
want.

What operators usually mean by "don't slew there" is *"don't point
below the horizon"* (or below some atmospheric-refraction-safe
altitude floor). The local horizon is a **tilted great circle** on
the celestial sphere — it intersects the celestial equator at
`HA = ±6 h` and dips to alt `−(90° − LAT)` at the anti-meridian. A
rectangular Dec envelope can't track this circle. Two failure modes:

- **Too loose** with `dec_min_degrees = -90°` (the shipped default):
  the driver happily accepts a slew to `(HA = -3 h, dec = -40°)` at
  LAT 45°N — apparent altitude = `-4°` (below horizon, target
  invisible). The mount tracks toward the ground.
- **Too tight** with `dec_min_degrees = -25°`: the driver rejects a
  slew to `(HA = -3 h, dec = -30°)` at LAT 45°N — apparent altitude
  = `+4.5°` (legitimate above-horizon target).

The rectangular-Dec gate is the wrong shape for the constraint
operators actually care about. Phase 3 replaces it with a
**target-altitude floor** computed from HA + dec + site latitude.

### 3.1 Math

Standard spherical-astronomy formula:

```
sin(target_alt) = sin(LAT) · sin(target_dec)
                + cos(LAT) · cos(target_dec) · cos(target_HA)
```

The check rejects the slew if `target_alt < min_altitude_degrees`.

Worked examples at LAT 45°N (reproducing the cases from the
2026-05-15 planning conversation):

| Target | HA | dec | alt | Decision (floor = 0°) |
|---|---|---|---|---|
| Meridian, alt 5° | 0 h | −40° | +5° | Accept |
| East horizon at equator | −6 h | 0° | 0° | Accept (edge) |
| 5° east of meridian, alt 5° | −0.5 h | −39.4° | +5° | Accept |
| Low west, mid-dec | −3 h | −40° | −4° | **Reject** |
| Low west, higher dec | −3 h | −30° | +4.5° | Accept |
| North celestial pole | any | +90° | +45° | Accept (always) |

### 3.2 Coordinate-math helper

New pure function in
`services/star-adventurer-gti/src/coordinates.rs`:

```rust
/// Compute the target's local altitude in degrees from the
/// target's mechanical hour-angle (signed hours), declination
/// (degrees), and the observer's site latitude (degrees).
///
/// Pure spherical-astronomy formula:
/// `sin(alt) = sin(lat)·sin(dec) + cos(lat)·cos(dec)·cos(HA)`.
pub fn target_altitude_degrees(
    mech_ha_hours: f64,
    dec_degrees: f64,
    site_latitude_degrees: f64,
) -> f64 {
    let lat = site_latitude_degrees.to_radians();
    let dec = dec_degrees.to_radians();
    let ha = (mech_ha_hours * 15.0).to_radians();
    (lat.sin() * dec.sin() + lat.cos() * dec.cos() * ha.cos())
        .asin()
        .to_degrees()
}
```

Unit-tested directly against the worked examples above.

### 3.3 Envelope-check rework

Modify `MountDevice::check_within_safe_envelope` in
`services/star-adventurer-gti/src/mount_device.rs`:

1. Keep the existing RA mech-HA check (mechanical safety —
   counterweight clearance — unchanged from Phase 1.1).
2. Replace the rectangular Dec check with an altitude floor:
   - Compute `target_alt = target_altitude_degrees(mech_ha_hours,
     dec_degrees, self.config.site_latitude_deg)`.
   - If `target_alt < self.config.min_altitude_degrees`, reject
     with `INVALID_VALUE` and an error message naming the
     computed altitude.

Call-site change: the function currently takes `(ra_ticks,
dec_ticks, cpr_ra, cpr_dec)`. Phase 3 needs `(mech_ha_hours,
dec_degrees)` for the altitude computation in addition to (or
replacing) the encoder-tick pair. Callers already have both
quantities — see `slew_to_coordinates_async` lines 867-870 —
so the change is mechanical.

### 3.4 Configuration

```json
"mount": {
  ...
  "ra_min_hours": -6.95,
  "ra_max_hours": 6.95,
  "min_altitude_degrees": 0.0,
  ...
}
```

Field semantics:

- **`min_altitude_degrees: f64`** (default `0.0`) — reject slews to
  targets whose computed apparent altitude is below this floor.
  - `0.0` — geometric horizon. Most natural default: rejects
    below-horizon pointing while accepting everything at or above.
  - `5.0` or `10.0` — operator buffer for atmospheric refraction,
    light pollution near horizon, or local obstructions.
  - **Negative** values theoretically allow below-horizon pointing
    (e.g., parking the OTA pointing down for dust-cap operations
    or sky-flats with a closed observatory). Accepted but the
    driver logs `info!` at startup so it's discoverable in support
    transcripts.

Dropped from `MountConfig`:

- `dec_min_degrees` — was a no-op at the default; superseded.
- `dec_max_degrees` — same.

### 3.5 Migration

This is a **breaking config schema change**, even if most installs
won't notice:

- **Default config files** (no explicit dec envelope, just the
  defaults): the JSON no longer has `dec_min_degrees`
  / `dec_max_degrees` after upgrading. Behaviour change: targets
  below horizon previously accepted are now rejected (the right
  behaviour, but it *is* a change).
- **Config files with explicit Dec bounds**: serde rejects the
  unknown field on parse if we go strict-by-default. Either:
  - **Strict from the start**: rev the config schema version, ship
    a one-line migration note in the release notes ("remove
    `dec_min_degrees` / `dec_max_degrees`; optionally add
    `min_altitude_degrees`"). Cleanest if the user base is small.
  - **Deprecation window**: keep the fields in the schema for one
    release, emit a `warn!` log when present, and ignore the
    values. Drop them in the following release.

Pick strict-from-start; the GTi service has a single operator
right now and migration is trivial.

### 3.6 BDD scenarios

New scenarios in
`services/star-adventurer-gti/tests/features/slew.feature`:

- Slew to target *at* the south horizon (alt = 0°) at meridian is
  accepted at the edge (`min_altitude_degrees = 0`).
- Slew to target just below south horizon (alt = -0.1° at
  meridian, dec ≈ -45.1° at LAT 45°N) is rejected with
  invalid-value.
- Slew to east horizon at celestial equator (HA = -6 h, dec = 0°)
  is accepted (alt = 0).
- Slew to target above horizon at extreme dec (e.g., `(HA = -3 h,
  dec = -30°)` at LAT 45°N → alt +4.5°) is accepted.
- Slew with `min_altitude_degrees = 5°` rejects the alt +4.5°
  target — sanity check that the configured floor actually applies.
- Slew with `min_altitude_degrees = -45°` accepts a target near
  nadir — sanity check that negative floors are usable.

### 3.7 Test plan

- Unit tests for `target_altitude_degrees` against the worked
  examples in §3.1 (LAT 45°N) plus a few extreme cases (LAT 0°
  equator, LAT 90°N pole, southern-hemisphere LAT).
- Unit tests for `check_within_safe_envelope` confirming altitude
  rejection at the boundary (floor = 0°, target alt = -0.001° →
  reject; target alt = +0.001° → accept).
- Re-run the existing safety-gate unit tests — `slew_async_refuses_
  ra_outside_safe_envelope` etc. — and update the ones that exercise
  the Dec rectangle (`slew_async_refuses_dec_outside_safe_envelope`)
  to exercise the altitude floor instead.
- ConformU nightly: expect the same 7-issue baseline. The
  altitude floor is more permissive than the current `±90°` Dec
  envelope for in-envelope-but-below-horizon targets, but ConformU
  doesn't probe those.

### 3.8 Open questions

1. **Refraction handling.** Geometric horizon (alt = 0°) is the
   strict math answer; the *apparent* horizon (including
   atmospheric refraction) is at geometric alt ≈ -0.5° because the
   atmosphere bends low-altitude light up. For safety purposes
   geometric is more conservative (reject before the target
   visibly drops below horizon). For operator usability,
   refraction-corrected is friendlier. **Recommend geometric** —
   simpler, defensible, and users who want refracted can drop
   `min_altitude_degrees` to `-0.5`.

2. **Does the altitude check apply to `Park`?** Park is an
   encoder-target operation, not a celestial slew. The altitude
   computation needs HA and dec; for a parked encoder position
   those derive from the park encoder ticks + current LST. Two
   options: (a) skip the altitude check for park (treat park
   targets as mechanically-defined and trust the operator), or
   (b) compute the park target's celestial coordinates and apply
   the floor. Recommend (a) — Park is set by `SetPark` from the
   current position, which the operator chose explicitly.

---

## Open questions

1. **Per-mount-model envelope table** — where should it live? Adding
   a `mount_model` enum to `MountConfig` and a lookup table inside the
   service feels right, but the protocol crate is intentionally
   mount-agnostic. The envelope is a service-layer concern, so keep it
   there.

2. **Auto-flip default** — if Phase 2.5 lands, what's the right
   default for `auto_flip_during_tracking`? Conservative pick: `false`
   (host controls flip timing). Aggressive pick: `true`. Probably
   `false` for safety; document the trade-off.

---

## Resolved during planning (2026-05-15)

- **Mechanical clearance on the negative-`mech_HA` half of the
  encoder range.** Determined to be symmetric to the verified
  positive-direction tracking arc. Normal sidereal tracking of
  east-of-meridian targets sweeps the counterweight through the
  lower-arc mirror of the verified upper-arc, and no binding has been
  observed in real use. The unverified `mech_HA ∈ (-12, -6.95)` band
  is the mirror of `mech_HA ∈ (+6.95, +12)`; Phase 4's binding at
  `+9` would have a mirror at `-9` if it exists, but the mount's
  structural symmetry around the polar axis makes either-both-or-
  neither the only options. Folded into Phase 2.8 as
  first-real-flip-on-hardware validation rather than a separate
  pre-implementation test.

---

## References

- Issue #202 — Dec-encoder `SideOfPier` switch.
- Issue #223 — this plan's tracking issue.
- PR #220 — implementation + ConformU expected-issues refresh + the
  hardware-test session that produced Phase 1.1.
- Commit `b8e83bc` (2026-05-15) — Phase 1.1 landed.
- INDI eqmod source — `eqmodbase.cpp::Goto`,
  `eqmodbase.cpp::EncodersToRADec` (https://github.com/indilib/indi-3rdparty/tree/master/indi-eqmod).
- [Design doc §"Side-of-pier"](../../services/star-adventurer-gti.md#side-of-pier).
- [Design doc §"Phase 4 driver-logic changes that real hardware required"](../../services/star-adventurer-gti.md#phase-4-driver-logic-changes-that-real-hardware-required) — mechanical-stall finding that motivates the envelope's existence.
- [Design doc §"Running ConformU manually"](../../services/star-adventurer-gti.md#running-conformu-manually) — current expected-issues baseline.

# Plan: `polar-align` orchestrator plugin — plate-solving polar alignment

## Goal

Polar-align an equatorial mount using the imaging train itself — no
polar scope, no PoleMaster, no view of Polaris required beyond the pole
region being above the horizon. The operator starts a polar-alignment
session; the mount takes three images at different RA-axis positions;
each is plate solved; the three solved pointings determine where the
mount's RA axis points on the sky; the axis is compared against the
refracted celestial pole; and the operator is told how far to move the
azimuth and altitude adjusters — first as numbers, then (adjustment
phase) as a live PoleMaster-style display: circles mark where each
detected star will sit once the axis is on the pole, updating after
every solve while the operator turns the bolts.

This is the method N.I.N.A.'s Three Point Polar Alignment plugin and
(near the pole) SharpCap's polar alignment use; it is field-proven to
converge below 1 arcminute with ordinary imaging FOVs.

The primary beneficiary on the reference rig is the Star Adventurer
GTi — a portable mount that is re-polar-aligned every session and has
no encoders. Encoders are irrelevant here: plate solves measure what
the mount actually did, not what it claims.

## Background

Nearly every primitive already exists:

- `plate-solver` (rp-managed ASTAP wrapper) + rp's `plate_solve` MCP
  tool — solves return center, pixel scale, and rotation; hinted solves
  run in ~100 ms.
- rp MCP mount tools: `slew`, `set_tracking`, `get_tracking`, `unpark`,
  `get_park_state`, `abort_slew`, plus the mount motion gate.
- rp MCP camera tools: `capture` (returns `image_path` on the shared
  filesystem + `document_id`).
- The orchestrator-plugin pattern (`calibrator-flats` is the template):
  `POST /invoke`, MCP client back to rp via `rp-mcp-client` (ADR-017),
  completion POST to `/api/plugins/{workflow_id}/complete`, cleanup
  guard on failure.
- `rp-ephemeris` (erfars/ERFA) for the ICRS → observed conversion
  (precession-nutation, aberration, refraction).

The genuinely new work: the axis-finding geometry, a small star
detector, one extension to plate-solver's solve response (full WCS:
CRPIX + CD matrix, which also settles image parity), and the plugin
service itself.

## Decisions (settled during design discussion, 2026-08-01)

### D1 — Measurement position: near the pole, deliberately offset in dec

Default measurement declination is **85°** (mirrored, −85°, for
southern-hemisphere sites), not the pole itself and not the celestial
equator:

- The three solved centers sweep a small circle of radius θ (the
  scope-to-axis angle) around the RA axis. The circle-fit error
  amplification depends on the **arc angle spanned** (fixed at 90° by
  the sweep), not on θ — the geometry is scale-invariant — so
  near-pole is not ill-conditioned. The only true degeneracy is
  θ collapsing toward the solve noise floor, which happens exactly at
  dec 90 where θ becomes the (uncontrolled, possibly near-zero) cone
  error. Dec 85 makes θ a deliberate ~5°.
- Near-pole keeps all three exposures at nearly the same altitude, so
  differential refraction cancels instead of needing precise modeling
  (an equator sweep spans wildly different altitudes).
- The adjustment-phase geometry is optimal pointing north: the pole
  region is ~90° from both adjuster axes (vertical for azimuth,
  horizontal east–west for altitude), so both bolts produce maximal,
  decoupled image motion. Pointing due east/west makes altitude error
  unobservable (motion degenerates to field roll) — N.I.N.A. TPPA's
  FAQ warns about exactly this.
- Only north-facing sky is needed (relevant for portable GTi setups).

The start position is configurable; a TPPA-style "start from current
position" mode is future work (P6).

### D2 — Axis from three solved centers (plane normal), roll as cross-check later

Three unit pointing vectors on a small circle about the RA axis span a
plane whose unit normal **is** the axis direction:

```
K = normalize((p2 − p1) × (p3 − p2)),  sign chosen toward the visible pole
```

This uses only solved centers — no roll, no parity — and at dec 85
with 45° spacing is conditioned to ~3× the per-solve error (~10–15″
with ordinary solves). The attitude-based formulation (extract the
rotation axis of the relative rotation between full camera attitudes)
works anywhere in the sky including arbitrary start positions, but
requires roll + parity; it landed in P6 (see D9) alongside
start-from-current-position, and the method that is not primary for
the configured mode now serves as a free cross-check on the other.

The **adjustment phase**, however, needs full attitudes from day one
(D4): a single solve must reveal how the axis moved since the
measurement, and that update is `K1 = R · K0` where `R` is the
relative rotation between full camera attitudes. Rotation about the RA
axis (tracking) commutes out of this update — `R_track · K0 = K0` — so
tracking stays on during adjustment and never corrupts the axis
estimate.

### D3 — Error expressed in observed alt/az; refraction rides the axis conversion

Plate solves map *apparent* (refracted) pointings to catalog
coordinates — the solver pulls each measured center down by the
refraction at its altitude. The fitted axis therefore already carries
that pull, and converting it ICRS→observed **with the refraction
model on** re-adds the lift: a perfectly aligned axis lands exactly
on the *geometric* pole. The target is thus the plain (azimuth 0,
or 180 in the south; altitude |site latitude|) point with **no
refraction term of its own** — the ~1′ refraction correction at mid
latitudes lives entirely in the axis conversion. (Polar motion ≤0.3″
and vertical deflection are ignored.) A config toggle
(`refraction.enabled`, default true) removes the model from the
conversion so synthetic-geometry tests can assert exact values; the
target stays the same either way.

Output: azimuth error and altitude error in arcminutes, each with an
explicit adjuster direction ("move azimuth east", "lower altitude").

### D4 — plate-solver returns the full WCS; parity rides the CD matrix

The solve response gains CRPIX and the 2×2 CD matrix (deg/pixel).
The CD determinant's sign encodes parity (mirror-flipped optical
trains), retiring the rotation-without-parity ambiguity — the classic
polar-alignment-tool bug — and giving the plugin the full sky↔pixel
mapping it needs to project target circles. The existing scalar fields
(`pixel_scale_arcsec`, `rotation_deg`) stay for compatibility. The
`.wcs` sidecar already contains these keys; the wrapper just surfaces
them.

### D5 — Adjustment UX: circles when the error fits in-frame, arrow otherwise

PoleMaster-style endgame: after measurement the plugin loops
capture → hinted solve → detect brightest N stars → compute each
star's target pixel (where it lands once the axis is on the pole) →
publish via `GET /status` (JSON: phase, error vector, star/target
pixel pairs, image path). Because the camera is rigidly attached to
the mount head, the aligned-state pixel positions are a closed-form
transform of the current solve — for small errors approximately a
uniform translation of the field, which is exactly the intuitive
"move the pattern into the circles" experience.

Our FOV is the imaging train's (~1–2°), not PoleMaster's 11°: when the
error exceeds roughly half the FOV the targets fall off the sensor, so
`/status` carries both representations and flags `in_frame`; the UI
(P5) shows an arrow until circles become possible. Every frame is
absolutely re-solved, so the loop is self-healing — big adjustments
just pick new stars.

The plugin serves data (JSON + the solved frame's path); rendering
belongs to ui-htmx (P5) as an SVG overlay. No image re-encoding in the
plugin in v1.

### D6 — Slews stay on one pier side; only the RA axis moves

The three measurement points sit at hour angles
`direction × (first_point_ha_deg + i · sweep_deg)`, i = 0..2 (defaults
15°, +45° steps → 15°/60°/105°, direction west), all on one side of
the meridian so a GoTo can never decide to meridian-flip
mid-measurement — a flip moves the dec axis and invalidates the
geometry. Slew targets are equal-dec coordinate slews computed from
LST (site longitude + wall clock); `slew` requires tracking on, which
the workflow enables after unparking.

### D7 — Site coordinates come from the plugin's own config in v1

The measurement needs site latitude (pole altitude, refraction) and
longitude (LST for slew targets). rp did not expose a
site-information MCP tool at MVP time, so `site.latitude_deg` /
`site.longitude_deg` were required plugin config. Sourcing them from
rp landed in Phase 8 — see D10; the config shape did not change (the
tool only fills the same numbers, and an explicit block still wins).

### D8 — Tenets

Tenet 3 (no actuation on connect) is honored: every slew happens
inside an operator-started session, exactly like calibrator-flats'
cover moves. Cleanup on failure or completion is stop-class only:
`abort_slew` if a slew is in flight, then leave the mount tracking at
its current position — never park, never slew back (a slew during
cleanup could itself fail and mask the original error). Tenet 1
(robustness): every capture/solve/slew is bounded by rp's own
deadlines; the adjustment loop is additionally bounded by
`adjustment.max_duration` so an abandoned session cannot hold the
mount forever.

### D9 — P6: attitude-based axis and current-position measurement (2026-08-01)

`measurement.mode` selects the sweep: `near_pole` (the D1 default,
unchanged) or `current_position` (TPPA-style start-from-anywhere).
Decisions specific to the mode:

- **Attitude extraction.** Each measurement solve's `wcs_matrix`
  yields a full camera attitude; the relative rotation between
  consecutive attitudes is a pure rotation about the RA axis
  (commanded sweep + tracking, same physical axis), so its rotation
  axis — skew-symmetric part, angle `atan2(|skew|/2, (tr−1)/2)` —
  is the measured axis. Guards: each segment must rotate ≥ ~1° (a
  smaller rotation means the mount ignored the slew and the
  extraction would amplify solve noise); a rotation within numerical
  noise of 180° has an ambiguous axis and is rejected (and is itself
  flip-shaped); the two sign-aligned segment axes must agree within
  1°, since a disagreement means something other than the RA axis
  moved (meridian flip, bumped tripod). Survivors are averaged, sign
  toward the visible pole.
- **Mount-frame targets.** `get_mount_position` anchors the sweep;
  all targets keep the mount's *reported* declination. Commanding
  the dec the mount already believes is what makes the dec axis
  provably stationary — anchoring on a solved position instead
  would command a dec differing by the pointing error, moving the
  dec axis by roughly the misalignment being measured.
- **Sweep direction is automatic**: away from the meridian, on the
  side the mount already stands (sign of the current hour angle), so
  an RA-only sweep can never cross the meridian and invite a GoTo
  flip. `direction`, `dec_deg`, and `first_point_ha_deg` are unused
  in this mode.
- **First point in place.** The first exposure is taken where the
  mount stands; only points 2 and 3 slew.
- **Full WCS required.** A matrix-less measurement solve aborts in
  this mode (no attitude, no axis) with an error naming the point.
- **Horizon guard, both modes.** Any measurement target below 10°
  observed altitude aborts before any motion — near-horizon solves
  are refraction-dominated garbage. (Near-pole sweeps at temperate
  latitudes sit far above this; the guard mostly protects arbitrary
  current-position sweeps and tropical near-pole geometry.)
- **Cross-check surfaced.** Both methods run whenever their inputs
  exist; the non-primary axis's angular separation from the primary
  is published as `measurement.cross_check_arcsec` (omitted when
  unavailable, warned above 2′). The BDD choreography synthesizes
  attitude-consistent per-point CD matrices via `wcs_from_attitude`
  (the exact inverse of `attitude_from_wcs`), so the cross-check is
  asserted end-to-end, not just in unit tests.

### D10 — Site coordinates resolve config-first, then from rp (2026-08-01)

D7 carried the site in required plugin config because rp exposed no
site-information tool. That gap closes from the rp side:

- rp gains a **`get_site`** MCP tool (planner category, where every
  other site-consuming tool lives) returning the configured `site`
  block — `latitude_degrees`, `longitude_degrees` — or the planner
  tools' existing structured "site not configured" error when rp has
  no site. rp's site is already validated against the mount's
  reported `SiteLatitude`/`SiteLongitude` on connect (hard error
  above 0.01°, rp.md §Site Validation Against the ASCOM Mount), so a
  mount-backed rig gets mount-vetted numbers with no new mount read.
- The plugin's `site` block becomes **optional**. Resolution happens
  once per workflow, config first: an explicit `site` wins (a
  portable rig whose plugin config travels with the tripod), else
  `get_site`. rp-sourced values pass the exact same newtype
  validation as config values (range plus the ≥1°-from-the-equator
  rule), with the error naming rp as the source; no site on either
  side aborts the workflow naming both fixes.
- D7's shape promise holds: a configured `site` block is unchanged,
  and sourcing only fills the same two numbers. The declination
  hemisphere fold and LST move from config-load time to workflow
  start, where the resolved site lives.

### D11 — Manual-rotation mode for non-GoTo trackers (2026-08-01)

`measurement.mode = "manual_rotation"`: the operator rotates the RA
axis by hand between exposures — for trackers with no GoTo (plain
Star Adventurer, sky trackers) where rp has no mount at all.

- **No mount tool is called in this mode** — no park/tracking
  preflight, no slew, no `get_mount_position`, and no `abort_slew`
  cleanup (the plugin never commanded motion, so there is nothing
  stop-class to stop). A manual-only registration needs only the
  camera, solve, and site tools.
- Flow: capture point 1 where the tracker points → publish
  `awaiting_point: 2` on `/status` → the operator rotates the RA
  axis (15–45° recommended) and posts **`/measure/continue`** →
  capture → `awaiting_point: 3` → rotate → post → capture → the
  P6 attitude-based axis with the same guards as
  `current_position` (`wcs_matrix` required per point; the ≥1°
  segment guard doubles as "the operator actually rotated"; near-180°
  and segment-disagreement and parity guards unchanged; plane-normal
  cross-check best-effort).
- Each wait is bounded by `measurement.manual_timeout` (default
  10 m); expiry aborts the workflow naming the point, so an
  abandoned session cannot hold the camera forever.
- `/measure/continue` returns 202 while a wait is active, 409
  otherwise. The wait signal is re-armed per wait, so a stale or
  double-clicked post cannot skip the next point.
- **All three solves are blind** (`blind: true`, no
  pointing hint): the plugin has no trustworthy prediction of where
  the operator left the axis, and a wrong hint is worse than none.
  Manual rigs should budget `solve.timeout` for blind solves.
- No horizon guard: the plugin commands no motion, and the operator
  standing at the tripod owns the pointing. Tracking is neither read
  nor set — a running tracker only adds rotation about the very axis
  being measured, which the attitude extraction absorbs.
- `dec_deg`, `first_point_ha_deg`, `sweep_deg`, and `direction` are
  unused in this mode.

### D12 — PNG preview endpoint (2026-08-01)

`GET /preview.png` renders the most recent captured frame
(measurement or adjustment) as an 8-bit grayscale PNG, so the P5 UI
can put the actual sky behind the star/target overlay:

- Stride subsample to the requested `?width` (default 1024, clamped
  to [64, native]); linear percentile stretch (0.5–99.9%, computed
  on the preview pixels); a constant frame maps to mid-gray instead
  of dividing by zero. 404 before any capture and when the frame no
  longer exists on disk.
- This supersedes the MVP's "no image re-encoding in the plugin"
  line deliberately: rp's only pixel route is ASCOM
  `application/imagebytes` (for programmatic clients, not browsers),
  ui-htmx may run on a different host than the shared filesystem,
  and the plugin already sits next to the frames it captured.
  The boundary that stays is image *analysis*: star detection
  remains rp's `detect_stars`; the preview is presentation only.
- Pure Rust (`rp-fits` reader + the `png` encoder crate) — no native
  library, no new platform concerns.
- Downscaling is transparent to overlay accuracy: the UI draws the
  overlay in native pixel coordinates (`/status` star pairs) over a
  bitmap scaled to the same viewBox, so the preview's resolution
  never enters the math.

## MVP scope (this PR)

- Plan + design docs.
- plate-solver solve-response extension (D4) with tests and doc
  update.
- `polar-align` service: `/invoke`, `/health`, `/status`,
  `/adjust/finish`, measurement workflow (unpark → tracking on →
  3 × (slew, capture, solve) → axis + error), adjustment loop
  (capture → solve → axis update → star targets), completion report,
  doctor subcommand, SCM support, workspace/Bazel/packaging/doctor
  integration.
- Math module with exhaustive unit tests against synthetic
  misalignments (both hemispheres, refraction on/off, parity both
  ways).
- Star positions for the overlay come from rp's existing
  `detect_stars` tool — no plugin-side image processing.
- A small additive `rp-ephemeris` extension: refraction-optional
  ICRS→observed conversion and its inverse (needed for the pole
  target and the horizontal-frame correction rotation).
- BDD suite: OmniSim (telescope + camera) + rp + polar-align + an
  in-test plate-solver stub returning choreographed solves; doctor and
  TLS/auth smoke features.

Deferred:

- **P5 — ui-htmx page**: live SVG overlay (circles/arrow), phase
  display, finish button, driven from `/status`.
- **P7 — hardware validation on the rig** (GTi + SV605CC), then a
  README recipe.

## Phases

### Phase 1 — Design doc

`docs/services/polar-align.md`: architecture, the full measurement and
adjustment contracts (happy path + every error path), `/status` wire
format, configuration table, MVP boundary. This plan's D1–D8 are the
decision record; the design doc is the behavioral spec.

### Phase 2 — plate-solver WCS extension

The solve response gains one nested optional field:

```json
"wcs_matrix": {
  "crpix1": 512.0, "crpix2": 384.0,
  "cd1_1": -2.91e-4, "cd1_2": 1.2e-6,
  "cd2_1": 1.1e-6, "cd2_2": 2.91e-4
}
```

CRPIX in FITS 1-based pixel convention, CD in degrees/pixel as read
from the sidecar. All-or-nothing: `null` when the sidecar lacks a
complete CRPIX + CD set (no synthesis from CROTA2/CDELT — a
synthesized matrix would fabricate parity). The existing scalar
fields are unchanged. The field propagates in lockstep through:
`services/plate-solver` (`runner/wcs.rs` parse, `SolveOutcome`,
`SolveResponseBody`), `crates/rp-plate-solver` (rp's client),
rp's `plate_solve` MCP output and persisted `wcs` section,
`crates/bdd-infra`'s `PlateSolverStub`/`CannedWcs`, `mock_astap`'s
canned sidecar, and the plate-solver + rp design docs.

### Phase 3 — BDD scenarios (`@wip` until Phase 4 lands)

Features:

- `polar_alignment.feature` — measurement happy path against
  choreographed solves (three slews on one pier side, completion
  report carries the injected axis error within tolerance), solve
  failure aborts with `status: error` and a stop-class-only cleanup,
  parked mount is unparked first, tracking is enabled.
- `adjustment.feature` — `/status` publishes phase transitions,
  star/target pairs and `in_frame`; `/adjust/finish` completes the
  workflow; `adjustment.max_duration` completes it autonomously.
- `doctor.feature`, `auth.feature` — the shared smoke suites.

### Phase 4 — Implementation

Math module first (pure, unit-tested), then star detector, then the
service on the calibrator-flats template, then de-`@wip` the BDD
suite.

### Phase 5 — ui-htmx polar-alignment page (separate PR)

### Phase 6 — attitude math + arbitrary start position (landed; D9)

### Phase 7 — rig validation (GTi), README recipe, plan archive

### Phase 8 — site from rp, manual rotation, PNG preview (landed; D10–D12)

## Module structure

```
services/polar-align/src/
  main.rs            CLI entry (clap + doctor subcommand + ServiceRunner)
  lib.rs             ServerBuilder / BoundServer (calibrator-flats shape)
  config.rs          PolarAlignConfig (server block, site, measurement, adjustment)
  error.rs           thiserror error type
  routes.rs          POST /invoke, GET /health, GET /status, POST /adjust/finish
  mcp_client.rs      rp-mcp-client wrapper: capture/plate_solve/slew/tracking/park tools
  workflow.rs        measurement + adjustment orchestration, cleanup guard
  math.rs            vec3/rot3 helpers, plane-normal axis, attitude from WCS,
                     alt/az error decomposition, target-pixel projection
  ephemeris.rs       ICRS→observed via rp-ephemeris; LST; refracted pole
  preview.rs         FITS → stretched grayscale PNG for GET /preview.png
```

## References

- Design doc — `docs/services/polar-align.md`
- Template plugin — `docs/services/calibrator-flats.md`
- Solver — `docs/services/plate-solver.md`, ADR-005
- MCP client policy — ADR-017
- Typed quantities — ADR-006 (`math.rs` uses degree/radian newtypes)
- N.I.N.A. TPPA FAQ (method precedent):
  <https://github.com/isbeorn/nina.plugin.polaralignment/blob/master/PolarAlignment/FAQ.md>

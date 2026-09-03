# polar-align — Plate-Solving Polar Alignment Orchestrator

## Overview

`polar-align` is an orchestrator plugin that measures how far an
equatorial mount's RA axis is from the refracted celestial pole and
guides the operator through correcting it. It captures and
plate-solves an image at three RA-axis positions — slewing near the
pole by default, sweeping outward from wherever the mount already
points in current-position mode, or waiting for the operator to
rotate a non-GoTo tracker by hand in manual-rotation mode — computes
the axis direction from the three solves, then enters a live
adjustment phase: it keeps capturing and solving while the operator
turns the mount's azimuth/altitude adjusters, publishing the
residual error and PoleMaster-style star/target-circle pairs after
every solve, plus a rendered PNG of the latest frame for the UI to
draw them over.

The method is N.I.N.A. Three Point Polar Alignment's: rotating only
the RA axis sweeps the camera pointing along a circle whose center is
the axis; plate solves measure that circle absolutely. See
`docs/plans/polar-align.md` for the decision record (measurement
geometry, refraction, adjustment math).

### Tenets

1. **Measure absolutely, every frame.** Every image is plate-solved;
   nothing is tracked incrementally or template-matched. A failed
   solve skips one update; the next solve recovers the full state.
   Big corrections that push stars out of the frame are handled by
   construction — the next solve simply picks new stars.
2. **Only the RA axis moves between measurement exposures.** All three
   measurement points sit on one side of the meridian so a GoTo can
   never meridian-flip mid-measurement. A flip would move the dec
   axis and invalidate the geometry.
3. **Stop-class cleanup only** (project tenet 3). On failure or
   completion the workflow aborts any in-flight slew and leaves the
   mount tracking where it stands. It never parks and never slews
   back — a cleanup slew could itself fail and mask the original
   error, and the operator is at the mount anyway.
4. **The operator finishes the session.** Adjustment is interactive by
   nature; the loop runs until the operator posts
   `/adjust/finish` — bounded by `adjustment.max_duration` so an
   abandoned session cannot hold the mount and camera forever.

## Architecture

`polar-align` is a standalone HTTP service and a session-less MCP
client of `rp` (ADR-021). The operator starts a run with `POST /runs`;
the service connects to rp's MCP server at its configured
`mcp_server_url` and calls primitive tools. The browser/UI never talks
to the workflow directly — it polls `GET /status` (via ui-htmx in a
later phase), where the outcome also lands: nothing is posted back to
`rp`, which has no notion of a session (mcp-sessionless D6). (The
pre-D6 `POST /invoke` route, through which `rp` used to start runs,
went with plan slice 7.)

```
  operator / ui-htmx              polar-align (orchestrator)                rp (equipment gateway)
  ┌──────────────────┐            ┌──────────────────────────────┐          ┌───────────────────┐
  │ POST /runs ──────┼───────────►│ Measurement phase            │          │                   │
  │                  │            │  1. unpark + tracking on     │tool calls│  MCP server       │
  │                  │            │  2. 3× (slew, capture,       ├─────────►│  /mcp             │
  │                  │            │        plate_solve)          │          │                   │
  │                  │            │  3. axis + alt/az error      │          │                   │
  │ GET /status ◄────┼────────────┤ Adjustment phase             │          │                   │
  │ POST /measure/   │            │  4. loop: capture, solve,    │          │                   │
  │      continue ───┼───────────►│     update error + targets   │          │                   │
  │ POST /adjust/    │            │  5. finish → outcome on      │          │                   │
  │      finish ─────┼───────────►│     /status                  │          │                   │
  └──────────────────┘            └──────────────────────────────┘          └───────────────────┘
```

The solved images stay on the shared filesystem (`rp.md` §"File
Accessibility"); `/status` carries their paths, never pixels.

### Port

11172 (configurable) — in the orchestrator-plugin range next to
`calibrator-flats` (11170) and `session-runner` (11171).

## MCP Tools Used

| Tool | Usage |
|------|-------|
| `get_site` | Resolve the observer site when the plugin config omits its `site` block (rp's configured site, cross-checked against the mount on connect when the mount reports one) |
| `get_park_state` | Read `at_park` before any motion; decide whether to unpark |
| `get_mount_position` | Read the mount-frame pointing that anchors a current-position measurement (RA-only targets are computed relative to it) |
| `unpark` | Clear `AtPark` (no motion) before enabling tracking |
| `get_tracking` | Read tracking state and `can_set_tracking` |
| `set_tracking` | Enable sidereal tracking (required by `slew`; keeps the field quasi-static during adjustment) |
| `slew` | Move to each measurement point (equal-dec, one pier side) |
| `abort_slew` | Cleanup only: stop an in-flight slew on failure |
| `capture` | Take the measurement and adjustment exposures |
| `get_camera_info` | Read the sensor bounds the overlay's `in_frame` flag is computed against (preflight) |
| `plate_solve` | Solve each capture (hinted with the commanded pointing; blind in manual-rotation mode) |
| `detect_stars` | Locate the brightest stars in each adjustment frame for the target-circle overlay |

`manual_rotation` mode calls **no mount tool at all** — a manual-only
rig needs only `get_site` (when the plugin config carries no site),
`capture`, `get_camera_info`, `plate_solve`, and `detect_stars`.

## Runs

### `POST /runs`

Starts a polar-alignment run. Answers `202 Accepted` with
`{ "run_id": "run-…" }` once the workflow slot is reserved and the run
spawned; `409 Conflict` while a workflow is measuring or adjusting (the
plugin drives a single mount and camera; two concurrent alignments are
meaningless); `400 Bad Request` when the config carries no
`mcp_server_url`. The MCP connection is made on the run task, so an
unreachable `rp` surfaces on `/status` as `phase: "error"`. The run's
outcome is `/status` — there is no completion callback.

## Behavioral Contracts

### Measurement phase

0. Resolve the observer site: the config's `site` block when
   present, else rp's `get_site` tool. rp-sourced coordinates pass
   the same validation as configured ones (range, ≥1° from the
   equator), with the error naming rp as the source; no site on
   either side aborts naming both fixes ("set `site` in the
   polar-align config or configure rp's `site` block").
1. `get_park_state`. If `at_park` and `can_unpark`: `unpark`. If
   `at_park` and not `can_unpark`: abort with an error naming the
   condition (nothing has moved). *Skipped in `manual_rotation`
   mode, which touches no mount tool.*
2. `get_tracking`; if tracking is off and `can_set_tracking`:
   `set_tracking(true)`. Off and not settable: abort (rp's `slew`
   would fail anyway; failing here gives a clearer message).
   *Skipped in `manual_rotation` mode.*
3. Compute the three measurement targets, by `measurement.mode`:
   - **`near_pole`** (default): local sidereal time from the
     resolved site longitude; target hour angles
     `direction × (first_point_ha_deg + i × sweep_deg)` for
     i = 0, 1, 2; declination `measurement_dec_deg` (sign follows
     the site hemisphere); RA = LST − HA, folded to [0, 24h).
   - **`current_position`**: `get_mount_position` anchors the sweep.
     All three targets keep the mount's own reported declination —
     commanding the mount-frame declination it already reports is
     what guarantees the dec axis never moves — and step
     `sweep_deg` apart in RA, in the direction that moves the
     hour angle *away* from the meridian (a sweep toward the
     meridian could trigger a GoTo meridian flip mid-measurement).
     `dec_deg`, `first_point_ha_deg`, and `direction` are unused in
     this mode.
   - **`manual_rotation`**: no targets are computed — the operator
     rotates the RA axis by hand between exposures. `dec_deg`,
     `first_point_ha_deg`, `sweep_deg`, and `direction` are all
     unused.
   Any computed target below 10° observed altitude aborts before any
   motion, naming the point and its altitude — a near-horizon
   exposure gives refraction-dominated garbage. (`manual_rotation`
   computes no targets, commands no motion, and therefore has no
   horizon guard: the operator at the tripod owns the pointing.)
4. For each point: `slew` → settle → `capture`
   (`measurement.exposure`) → `plate_solve` with
   `pointing_hint` = the commanded coordinates and
   `search_radius_deg` = `solve.search_radius_deg`. In
   `current_position` mode the first point is captured where the
   mount already stands — only points 2 and 3 slew. In
   `manual_rotation` mode nothing ever slews: the first point is
   captured in place, and before each further point the workflow
   publishes `awaiting_point` on `/status` and waits — bounded by
   `measurement.manual_timeout` — for the operator to rotate the RA
   axis (15–45° recommended) and `POST /measure/continue`; a timeout
   aborts naming the point. Manual-mode solves are **blind** (no
   pointing hint, no mount hints): the plugin has no trustworthy
   prediction of where the operator left the axis, and a wrong hint
   is worse than none — budget `solve.timeout` accordingly. In both
   `current_position` and `manual_rotation` modes every measurement
   solve must carry a `wcs_matrix`; a matrix-less solve aborts with
   an error naming the point (these modes' axis needs full camera
   attitudes).
5. Axis, by mode: `near_pole` fits the plane normal of the three
   solved centers (sign toward the visible pole); a degenerate sweep
   (centers closer than ~2 arcsec — the mount didn't move) aborts
   with a distinct error. `current_position` and `manual_rotation`
   extract the common rotation axis of the relative rotations
   between the three camera attitudes, which works anywhere in the
   sky (and the ≥1° segment guard doubles as "the operator actually
   rotated" in manual mode). Whichever method is not primary runs as
   a best-effort cross-check: the angular separation between the two
   axes is published as `measurement.cross_check_arcsec` and warned
   about above 2′ (see the plan's D2/D9 and the Geometry Reference).
   The axis is converted to observed azimuth/altitude; error against
   the refracted pole (D3).
6. Phase transitions to `adjusting`; the measurement result is
   published on `/status`.

A failure at any step moves `/status` to `phase: "error"` with a
message naming the step, after stop-class cleanup (tenet 3):
`abort_slew` if and only if a slew was in flight.
`manual_rotation` skips the `abort_slew` — the plugin never
commanded motion, so there is nothing stop-class to stop (and a
manual-only rig may register no mount tools at all).

### Adjustment phase

Loop until `/adjust/finish` or `adjustment.max_duration`:

1. `capture` (`adjustment.exposure`) → `plate_solve` hinted with the
   previous solve's center.
2. Camera attitude from the solve's full WCS (center + CD matrix,
   parity included). Axis update `K ← R · K_prev` where `R` is the
   relative rotation between the previous and current attitudes —
   sidereal tracking rotates about the axis itself and therefore
   drops out of the update; only adjuster motion moves `K`.
3. Recompute the alt/az error. Detect stars via rp's `detect_stars`
   tool and keep the `star_count` brightest unsaturated ones; for
   each, compute its target pixel — where it will sit when the axis
   is on the refracted pole — via the correction rotation applied to
   the current attitude.
4. Publish everything on `/status`.

A failed solve (moving mount blurs stars while the operator turns a
bolt) is expected: the iteration is skipped, `/status.last_solve`
reports `failed`, and the loop continues. `consecutive_solve_failures`
≥ `adjustment.max_solve_failures` aborts the workflow — the sky may
have clouded over.

`/adjust/finish` (or the deadline) moves `/status.phase` to
`complete`, preserving the final measurement for display; the next run
resets it. Tracking is
left on, mount in place — the operator typically proceeds straight
into a normal imaging session.

### `GET /status`

The live contract for UIs. Always available; before any invocation it
reports `"phase": "idle"`.

```json
{
  "phase": "adjusting",
  "workflow_id": "wf-550e8400-e29b-41d4",
  "measurement": {
    "axis_azimuth_deg": 0.35,
    "axis_altitude_deg": 47.61,
    "azimuth_error_arcmin": 21.0,
    "altitude_error_arcmin": -12.4,
    "total_error_arcmin": 24.4,
    "azimuth_direction": "move azimuth west",
    "altitude_direction": "raise altitude",
    "cross_check_arcsec": 8.3,
    "measured_at": "2026-08-01T21:14:03Z"
  },
  "adjustment": {
    "updated_at": "2026-08-01T21:15:11Z",
    "image_path": "/data/rp/images/pa-000042.fits",
    "in_frame": true,
    "stars": [
      { "x": 512.3, "y": 388.1, "target_x": 498.7, "target_y": 401.0 }
    ],
    "last_solve": "ok",
    "consecutive_solve_failures": 0,
    "iterations": 17
  },
  "error": null
}
```

- `phase`: `idle` | `measuring` | `adjusting` | `complete` | `error`.
- `workflow_id`: the run id (`POST /runs`); `null` before the first
  run.
- `awaiting_point` (omitted unless set): the measurement point
  number (2 or 3) a `manual_rotation` workflow is waiting on. While
  present, the workflow is paused until `POST /measure/continue` or
  `measurement.manual_timeout` expires. Never present in the other
  modes.
- `measurement` appears from the end of the measurement phase onward
  and is updated by every adjustment solve (the error shrinks as the
  operator converges). Signed errors: azimuth positive = axis east of
  the pole, altitude positive = axis above it; the `*_direction`
  strings state the corrective adjuster motion in plain words.
- `cross_check_arcsec` is the angular separation between the primary
  axis and the alternate method's axis (plane-normal vs
  attitude-based, whichever is not primary for the mode). It is a
  measurement-phase result — adjustment solves do not recompute it —
  and is omitted when the alternate method had nothing to work with
  (e.g. matrix-less solves in `near_pole` mode). A large value means
  bad solves or a mount that moved more than its RA axis.
- `adjustment.in_frame` is false when the total error exceeds what the
  sensor can show (targets would fall outside the frame); UIs show an
  arrow from the numbers instead of circles.
- `stars` pairs each detected star's current pixel with its aligned
  target pixel, in 0-based pixel indices of `image_path` (the
  convention `detect_stars` reports; the WCS math is FITS 1-based
  internally and the service converts at that boundary).
- `error` carries the failure message when `phase` is `error`, null
  otherwise.

### `POST /measure/continue`

Confirms a manual rotation: the operator has rotated the RA axis and
the tracker has settled, so the paused `manual_rotation` measurement
may capture the next point. Returns `202 Accepted` while a wait is
active (`/status.awaiting_point` present), `409 Conflict` otherwise —
including in the other measurement modes, which never wait. The wait
signal is armed per wait, so a duplicate or late post cannot skip a
later point.

### `GET /preview.png`

The most recent captured frame (measurement or adjustment), rendered
as an 8-bit grayscale PNG for the UI to draw the star/target overlay
over:

- `?width=` selects the preview width in pixels; default 1024,
  clamped to [64, native width]. Height follows the sensor aspect
  ratio. Downscaling is a stride subsample — the overlay stays in
  native pixel coordinates (`/status`), so preview resolution never
  affects overlay accuracy; the UI scales the bitmap under its
  viewBox.
- Brightness is a linear percentile stretch (0.5–99.9%, computed on
  the preview pixels). A constant frame renders mid-gray.
- `404 Not Found` before any frame has been captured, and when the
  frame no longer exists on disk (rp owns the capture directory and
  may prune it).
- The preview is presentation-only: star *analysis* stays in rp's
  `detect_stars`, and the alignment math never touches these pixels.

### `POST /adjust/finish`

Ends the adjustment loop — `/status` moves to `phase: "complete"`,
keeping the final measurement block and the iteration count — and
returns `202 Accepted`.
Returns `409 Conflict` when no workflow is in the `adjusting` phase.

### `GET /health`

`200 OK` with a static body once the server is up (config validation
happens at load; there are no external resources to probe — the mount
and solver are reached through rp per-invocation).

## Configuration

The service reads a single JSON config file; `--config` names it,
otherwise the platform default (`~/.config/rusty-photon/polar-align.json`
on Linux, `%PROGRAMDATA%\rusty-photon\polar-align.json` on Windows)
via `rusty-photon-config`. Camera and mount ids are mandatory, so
the packaged systemd unit gates on the file with
`ConditionPathExists` (no built-in default config).
`deny_unknown_fields` throughout.

```json
{
  "server": { "port": 11172, "bind_address": "0.0.0.0", "tls": null, "auth": null },
  "mcp_server_url": "http://localhost:11115/mcp",
  "service_auth": null,
  "ca_cert": null,
  "camera_id": "main-cam",
  "mount_id": "mount",
  "site": { "latitude_deg": 48.1, "longitude_deg": -122.8 },
  "measurement": {
    "mode": "near_pole",
    "dec_deg": 85.0,
    "first_point_ha_deg": 15.0,
    "sweep_deg": 45.0,
    "direction": "west",
    "exposure": "2s",
    "settle": "2s",
    "manual_timeout": "10m"
  },
  "adjustment": {
    "exposure": "2s",
    "interval": "1s",
    "max_duration": "30m",
    "max_solve_failures": 10,
    "star_count": 10
  },
  "solve": { "search_radius_deg": 5.0, "timeout": "30s" },
  "refraction": { "enabled": true, "temperature_c": 10.0, "pressure_hpa": 1010.0 }
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `server` | object | `{ "port": 11172 }` | Shared `ServerConfig` (ADR-016) for `/runs`, `/status`, `/health` and the operator routes |
| `mcp_server_url` | string or null | null | `rp`'s MCP endpoint, used by every `POST /runs` run; without it `/runs` answers `400` |
| `service_auth` / `ca_cert` | — | null | Credentials/CA toward rp, exactly as calibrator-flats (ADR-017) |
| `camera_id` | string | required | Camera on the imaging train used for alignment exposures |
| `mount_id` | string | required | The mount (informational; rp's mount tools address the singular configured mount) |
| `site` | object | optional | Observer site. When absent, resolved per workflow from rp's `get_site` tool (rp's configured, mount-validated site); an explicit block wins |
| `site.latitude_deg` | float | — | Geodetic latitude, degrees, north positive. Range ±90; `abs(latitude) < 1°` is rejected (no meaningful pole altitude). rp-sourced values pass the same rules |
| `site.longitude_deg` | float | — | Degrees, east positive, range ±180 |
| `measurement.mode` | `"near_pole"`\|`"current_position"`\|`"manual_rotation"` | `"near_pole"` | `near_pole` sweeps the configured dec-85 arc; `current_position` sweeps the RA axis from wherever the mount points, away from the meridian; `manual_rotation` waits for the operator to rotate a non-GoTo tracker between exposures |
| `measurement.dec_deg` | float | 85.0 | `near_pole` only. Measurement declination; the sign is folded to the resolved site's hemisphere at workflow start (the site may come from rp) |
| `measurement.first_point_ha_deg` | float | 15.0 | `near_pole` only. Hour angle of the first point, degrees from the meridian (1–60) |
| `measurement.sweep_deg` | float | 45.0 | Hour-angle step between points (10–60; total span ≤ 150° keeps one pier side) |
| `measurement.direction` | `"east"`\|`"west"` | `"west"` | `near_pole` only. Which side of the meridian the three points sit on; `current_position` picks the side the mount is already on |
| `measurement.exposure` | humantime | `"2s"` | Measurement exposure duration |
| `measurement.settle` | humantime | `"2s"` | Extra settle after each slew before capturing |
| `measurement.manual_timeout` | humantime | `"10m"` | `manual_rotation` only. Ceiling on each wait for `POST /measure/continue`; expiry aborts the workflow naming the point |
| `adjustment.exposure` | humantime | `"2s"` | Adjustment-loop exposure duration |
| `adjustment.interval` | humantime | `"1s"` | Pause between adjustment iterations |
| `adjustment.max_duration` | humantime | `"30m"` | Hard ceiling on the adjustment phase |
| `adjustment.max_solve_failures` | int | 10 | Consecutive failed solves that abort the workflow |
| `adjustment.star_count` | int | 10 | Brightest stars published with target circles |
| `solve.search_radius_deg` | float | 5.0 | Passed to `plate_solve` alongside the pointing hint. Not sent with `manual_rotation`'s blind solves — the radius bounds the search around a hint, and a blind solve has none |
| `solve.timeout` | humantime | `"30s"` | Per-solve timeout passed to `plate_solve` |
| `refraction.enabled` | bool | true | Apply refraction to the pole target and the axis conversion |
| `refraction.temperature_c` / `pressure_hpa` | float | 10.0 / 1010.0 | Refraction model inputs |

Range rules are enforced parse-don't-validate style (newtypes with
serde `try_from`, per `development-workflow.md`), so a bad config
fails at load naming the field. All range and cross-field rules
apply in every mode — including to fields the configured mode
ignores. The mode is a per-session choice: a latent near-pole
geometry error must fail at load, not on the night the operator
switches back to `near_pole`.

There is no rp-side registration: `rp` serves this service like any
other MCP client and rejects a `type: "orchestrator"` entry since
mcp-sessionless D11. The `rp` tools it calls are `capture`,
`get_camera_info`, `plate_solve`, `detect_stars`, `slew`, `abort_slew`,
`set_tracking`, `get_tracking`, `unpark`, `get_park_state`,
`get_mount_position` and `get_site`.

A `manual_rotation`-only rig (no GoTo, no rp mount) trims the list to
`capture`, `get_camera_info`, `plate_solve`, `detect_stars`, and —
when the plugin config carries no `site` block — `get_site`.

`polar-align doctor [--config <file>] [--json]` diagnoses the config
read-only without starting the service, per
[doctor.md §Per-service doctors](doctor.md).

## Geometry Reference

The math contract (implemented in `math.rs` / `ephemeris.rs`, unit
tests are the executable spec):

- **Axis from centers.** Unit vectors `p1, p2, p3` from the three
  solved centers; `K = normalize((p2 − p1) × (p3 − p2))`, sign flipped
  if `K` points away from the visible pole's hemisphere. Degenerate
  input (`|p_i − p_j|` under `min_point_separation_arcsec`, or a
  cross product below numeric floor) is an error, not a NaN.
- **Axis from attitudes.** The relative rotation between two camera
  attitudes taken with only the RA axis moving is a rotation about
  the axis itself (commanded sweep plus tracking, both about the
  same physical axis), so its rotation axis — from the matrix's
  skew-symmetric part, angle via
  `atan2(|skew|/2, (trace − 1)/2)` — *is* the RA axis. Three
  attitudes give two consecutive segments; each must rotate by at
  least ~1° (else the mount didn't move and the extraction is noise)
  and the two sign-aligned segment axes must agree within 1° — a
  disagreement means something other than the RA axis moved (a
  meridian flip, a bumped mount) and is an error. A rotation within
  numerical noise of 180° is rejected too: its axis is ambiguous,
  and a real near-180° relative rotation is itself flip-shaped.
  Consecutive attitudes must share parity — a rigid optical train
  cannot change its mirror state, so an improper relative transform
  (a solve lying about parity) is rejected rather than silently
  yielding a meaningless axis. The surviving segment axes are
  averaged, sign toward the visible pole.
  This works at any pointing, including ones where the solved
  centers barely separate — the camera *frame* still rotates by the
  full sweep even when the boresight barely moves.
- **Attitude from WCS.** Boresight from the solve's center; the
  solve response's `wcs_matrix` block (CRPIX + the 2×2 CD matrix,
  degrees/pixel, FITS conventions) gives the sky directions of the
  pixel axes on the tangent plane (ξ east, η north), orthonormalized
  into a rotation matrix. `det(CD) > 0` means a mirrored (flipped)
  image and is handled by construction — no separate parity flag in
  the math. A solve without `wcs_matrix` fails the adjustment
  iteration (it cannot yield an attitude); the measurement phase
  needs only centers.
- **Axis update.** `K ← (A_now · A_prev⁻¹) · K_prev`. Sidereal
  tracking is a rotation about `K` itself and cancels; adjuster
  motion is a rotation about a roughly horizontal axis and is what
  the update measures.
- **Observed conversion.** ICRS → observed alt/az via `rp-ephemeris`
  (ERFA), refraction per config. Pole target: azimuth 0 (north
  hemisphere) / 180 (south), altitude `|site.latitude_deg|` — with
  **no refraction term**: the solves already pulled the fitted axis
  down by refraction (apparent → catalog), so the refraction-on axis
  conversion re-adds it and a perfect axis lands on the geometric
  pole (the plan's D3 has the full derivation).
- **Targets.** Correction rotation `R_corr` = the rotation in the
  horizontal frame taking the axis onto the pole (azimuth rotation
  about the zenith, altitude rotation about the horizontal east–west
  axis — the two adjuster motions). Target pixel of a star at sky
  direction `s`: project `s` through the corrected attitude
  `R_corr · A_now`. `in_frame` = all targets within the sensor
  bounds reported by the solve's reference pixel geometry.

## Module Structure

```
services/polar-align/src/
  main.rs            CLI entry point (clap + tracing + doctor subcommand)
  lib.rs             ServerBuilder, BoundServer, module declarations
  config.rs          PolarAlignConfig + validated newtypes
  error.rs           Error types (thiserror)
  routes.rs          Axum router: /runs, /status, /health,
                     /measure/continue, /adjust/finish, /preview.png
  mcp_client.rs      rp-mcp-client wrapper (ADR-017)
  workflow.rs        Measurement + adjustment orchestration, cleanup guard
  math.rs            Axis, attitude, error decomposition, target projection
  ephemeris.rs       ICRS→observed, LST, refracted pole (rp-ephemeris)
  preview.rs         FITS → stretched grayscale PNG for /preview.png
```

Star *detection* is rp's `detect_stars` tool — the plugin carries no
image-analysis code of its own. `preview.rs` only re-encodes pixels
for display (`rp-fits` reader + the pure-Rust `png` encoder); nothing
it produces feeds the alignment math.

## Testing Strategy

Per `docs/skills/testing.md`.

- **Unit tests** carry the math: synthetic mounts with injected
  (azimuth, altitude) errors generate the three pointings by rotating
  a start vector about the misaligned axis; the module must recover
  the injected error to sub-arcsecond accuracy with refraction off,
  both hemispheres, both sweep directions, mirrored and unmirrored CD
  matrices. The attitude-based axis is held to the same bar through
  `wcs_from_attitude` — the exact inverse of `attitude_from_wcs` —
  which synthesizes per-point solves from attitudes rotated about a
  known axis, including from mid-sky pointings where the plane fit
  is poorly conditioned. Star detection is rp's, already tested there; the
  plugin's selection logic (brightest-N, saturated rejection) is
  unit-tested on canned `detect_stars` payloads.
- **BDD** carries the orchestration, with the full topology (OmniSim
  telescope + camera, rp, polar-align) plus an in-test plate-solver
  stub whose canned solves are choreographed from a known injected
  axis error — the `/status` measurement must recover it. Runs start
  with `POST /runs` and end on `/status`; nothing registers with rp.
  Scenarios per the plan's Phase 3 list; `doctor.feature` and `auth.feature` ride
  the shared smoke fixtures. Manual-rotation scenarios drive the
  wait/continue protocol over choreographed solves; the site-from-rp
  scenario runs a site-less plugin config against an rp whose `site`
  block matches OmniSim's default mount site (rp validates the two
  against each other on connect). The preview endpoint is asserted
  on the real FITS frames rp captures from the simulator.
- **Preview unit tests** render synthetic FITS written through
  `rp-fits` and decode the PNG back: dimensions follow the width
  clamp, the stretch spans the output range, a constant frame is
  mid-gray, and a missing file is a clean error.
- **No OmniSim image↔pointing coupling exists**, so end-to-end
  optical truth arrives only in Phase 7 rig validation.

## MVP Scope

In scope: everything above, including the P6 attitude-based axis and
`current_position` mode, and the Phase 8 additions — site-from-rp
sourcing (D10), `manual_rotation` mode (D11), and the PNG preview
endpoint (D12). Out of scope (see the plan): the ui-htmx page (P5).

## References

- Plan + decision record — `docs/plans/polar-align.md`
- Template plugin — `docs/services/calibrator-flats.md`
- Solver contract — `docs/services/plate-solver.md`
- rp's orchestration model — `docs/services/rp.md` §Orchestration
- ADR-016 server config, ADR-017 MCP client policy

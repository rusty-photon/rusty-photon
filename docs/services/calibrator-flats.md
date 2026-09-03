# calibrator-flats -- Calibrator Flat Calibration Orchestrator

## Overview

`calibrator-flats` is an orchestrator — an MCP client of `rp` — that
captures flat field correction frames using a stable light source (flat
panel, electroluminescent panel, or light box) controlled via an ASCOM
CoverCalibrator device. It iteratively determines the correct exposure
time per filter to achieve the target ADU level, then captures the
requested number of flat frames at that duration.

> **Document port — kept on purpose.** The same algorithm also ships as
> a `session-runner` workflow document
> (`services/session-runner/workflows/calibrator_flats.json`; see
> [`session-runner.md`](session-runner.md) § Example Documents), with
> this service's behavior as the oracle its tests pin. The document
> additionally cools the camera to its dark-library rung on the way in
> (`start_cooldown`, after `close_cover`) and warms it in its `finally`
> (`start_warmup`) — rp.md § Camera Cooling; this service leaves the
> cooler to whoever runs the night. **Both are kept, deliberately**
> (mcp-sessionless D13): the pair is the one reasonably simple procedure
> that exists as a Rust orchestrator and as a document, and their
> equivalence — the document's BDD suite runs this service's scenarios
> against the document — is the worked example for anyone deciding
> which form a new workflow should take. It is not turned into a tool
> provider either: a `take_flats` proxied tool would be a third
> implementation of the same procedure.

### Tenets

1. **Target 50% well depth.** Flat frames must have a median pixel value
   close to 50% of the camera's maximum ADU for optimal calibration
   quality. The target fraction is configurable.
2. **Automate the entire lifecycle, then put things back.** The
   orchestrator manages the CoverCalibrator (close cover, turn on light,
   turn off) so the user only needs to start the session — and it ends
   with the cover in the state it found it: a cover that was open when
   the session started is reopened at the end, one that was closed stays
   closed. (An anomalous initial reading — `Moving`, `Unknown`, `Error` —
   also ends closed: when the starting state cannot be known, covered is
   the state that protects the optics.)
3. **Safe cleanup on failure.** If the workflow fails at any point, the
   calibrator is turned off and the cover is opened before the
   orchestrator exits.
4. **Per-filter optimization.** Each filter has different throughput. The
   exposure time is found independently for each filter in the plan.
5. **Filterless rigs are first-class.** A one-shot-color camera has no
   filter wheel; `filter_wheel_id` is optional, and without one the plan
   entries are plain capture groups (no `set_filter` calls) — typically a
   single `{ "name": "OSC", "count": N }` entry whose `name` is only a
   label in logs and the `/status` result.
6. **Step the panel down when it is too bright.** A panel at full
   brightness can saturate the sensor even at the camera's shortest
   usable exposure, leaving the exposure search no gradient to descend.
   When the search exhausts its iterations still reading *over* the
   target, the orchestrator halves the panel brightness
   (`calibrator_on` at the new level) and searches again, down to a
   floor of 1. A search that ends *under* the target (panel too dim
   even at `exposure_max`) is not retried — dimming further cannot
   help — and falls back to capturing at the best duration found, with
   a warning, as before.

## Architecture

`calibrator-flats` is a standalone HTTP service and a session-less MCP
client of `rp` (ADR-021). An operator, `ui-htmx`, or a scheduler starts
a run with `POST /runs`; the service connects to `rp`'s MCP server at
its configured `mcp_server_url`, calls primitive tools, and reports the
outcome on its own `GET /status`. Nothing is posted back to `rp`, which
has no notion of a session (mcp-sessionless D6). (The pre-D6 `POST
/invoke` route, through which `rp` used to start runs, went with plan
slice 7.)

```
  operator / ui-htmx        calibrator-flats (orchestrator)      rp (equipment gateway)
  ┌──────────────┐          ┌───────────────────────┐            ┌───────────────────┐
  │ POST /runs ──┼─────────►│  1. close_cover       │            │                   │
  │              │          │  2. calibrator_on     │ tool calls │  MCP server       │
  │              │          │  3. per-filter loop:  ├───────────►│  /mcp             │
  │              │          │     find exposure     │            │                   │
  │              │          │     batch capture     │            │                   │
  │              │          │  4. calibrator_off    │            │                   │
  │ GET /status ◄┼──────────┤  5. open_cover        │            │                   │
  │              │          │  6. record outcome    │            │                   │
  └──────────────┘          └───────────────────────┘            └───────────────────┘
```

### Port

11170 (configurable)

## MCP Tools Used

The plugin calls these `rp` built-in MCP tools:

| Tool | Usage |
|------|-------|
| `get_camera_info` | Read `max_adu` to compute target ADU, read exposure limits for clamping |
| `capture` | Take exposures (both test exposures for calibration and final flat frames) |
| `compute_image_stats` | Measure median ADU of captured images for exposure time adjustment |
| `set_filter` | Switch filter wheel to the current filter in the plan |
| `get_cover_state` | Read the cover's state before any actuation, so cleanup can restore it |
| `close_cover` | Close the dust cover before starting flat calibration |
| `open_cover` | Reopen the dust cover at the end — only when it started open |
| `calibrator_on` | Turn on the flat panel at the configured brightness; re-light it at each halved level of the brightness ladder |
| `calibrator_off` | Turn off the flat panel when done |

## Runs

### `POST /runs`

Starts a flat-calibration run from the configured plan. Answers `202
Accepted` with `{ "run_id": "run-…" }` once the run is spawned; `409
Conflict` while a run is in progress (the service drives one panel and
one camera); `400 Bad Request` when the config carries no
`mcp_server_url` (there is no `rp` to reach). The MCP connection is made
on the run task, so a wrong or unreachable `rp` surfaces on `/status` as
`phase: "error"`, not on the start response.

### `GET /status`

```jsonc
{
  "phase": "complete",          // idle | running | complete | error
  "run_id": "run-…",            // the run id; rp's workflow_id on the legacy route
  "result": {                   // complete only — the same payload the legacy route posts
    "reason": "flat_calibration_complete",
    "filters_completed": [ { "filter": "Luminance", "duration": "1s 200ms",
                             "median_adu": 32100, "frames": 20, "converged": true } ],
    "total_frames": 20
  },
  "error": null                 // error only — the failure message
}
```

Before any run it reports `phase: "idle"`. A finished run's outcome stays
until the next run starts.

## Algorithm

### Full Workflow

```
connect to rp MCP server at mcp_server_url

# 1. Query camera capabilities
info = get_camera_info(camera_id)
target_adu = info.max_adu * target_adu_fraction

# 2. Record the cover's initial state, then prepare the flat panel
initial_cover = get_cover_state(calibrator_id)   # before any actuation;
                                                 # a read failure aborts here
close_cover(calibrator_id)
brightness = calibrator_on(calibrator_id, brightness)  # the applied level
                                                       # (device max when unset)

# 3. Capture flats per filter (or per capture group on a filterless rig)
for each filter in plan.filters:
    if plan.filter_wheel_id is set:
        set_filter(filter_wheel_id, filter.name)

    # 3a. Find the optimal exposure time for this filter, stepping the
    #     panel down whenever the search ends pinned over the target
    loop:
        # inner search: up to max_iterations captures
        duration = duration        # carried across brightness levels;
                                   # initial_duration on the first pass
        converged = false
        for iteration in 1..=max_iterations:
            result = capture(camera_id, duration)
            stats = compute_image_stats(result.image_path, result.document_id)

            deviation = |stats.median_adu - target_adu| / target_adu
            if deviation <= tolerance:
                converged = true
                break

            # Adjust proportionally
            if stats.median_adu == 0:
                duration = duration * 2           # guard division by zero
            else:
                duration = duration * (target_adu / stats.median_adu)

            # Clamp to camera limits
            duration = clamp(duration, info.exposure_min, info.exposure_max)

        if converged:
            break
        if stats.median_adu > target_adu and floor(brightness / 2) >= 1:
            # over-bright: dim the panel and search again (brightness ladder)
            brightness = floor(brightness / 2)
            calibrator_on(calibrator_id, brightness)
        else:
            # under-bright, or the ladder hit its floor — dimming cannot help
            log warning "exposure did not converge for filter {filter.name}"
            break

    # 3b. Capture the requested number of flat frames
    for i in 1..=filter.count:
        capture(camera_id, duration)

# 4. Clean up — restore the cover to its initial state
calibrator_off(calibrator_id)
if initial_cover == Open:
    open_cover(calibrator_id)
# a cover that started Closed (or read Moving/Unknown/Error) stays closed

# 5. Record the outcome on GET /status
{
  "phase": "complete",
  "run_id": "run-…",
  "result": {
    "reason": "flat_calibration_complete",
    "filters_completed": [...],
    "total_frames": N
  }
}
```

### Exposure Time Convergence

The iterative search uses proportional adjustment:

```
new_duration = current_duration * (target_adu / measured_median)
```

This converges quickly because the relationship between exposure time
and signal level is linear for a stable light source. Typically 2-3
iterations suffice. The algorithm handles edge cases:

- **Saturated image** (median >= max_adu): duration is reduced
  dramatically by the ratio.
- **Very dark image** (median ~0): duration is doubled as a fallback
  to avoid division by zero.
- **Already close**: if within tolerance on the first attempt, no
  iteration is needed.

A saturated sensor breaks the proportional step: the median pins at
max ADU no matter how short the exposure gets, so
`target / median ≈ 0.5` and the search degenerates to halving —
`max_iterations` passes cannot descend far from `initial_duration`,
and the frames carry no gradient to converge on. The **brightness
ladder** handles this: a search that exhausts its iterations still
reading *over* the target halves the panel brightness
(`calibrator_on` at `floor(brightness / 2)`) and runs again, carrying
the last duration forward (at the dimmer level the proportional step
re-adapts within a pass or two). The ladder starts from the applied
brightness `calibrator_on` reports (the configured `brightness`, or
the device maximum when unset), never re-brightens, persists across
filters (the level that worked for one filter is the starting point
for the next), and stops at a floor of 1 — at which point, or when
the search ends *under* the target (a panel too dim even at
`exposure_max`, where dimming further cannot help), the workflow
falls back to its old behavior: warn and capture at the best duration
found.

### Error Recovery

The workflow wraps the capture loop in a guard that ensures cleanup:

```rust
// Pseudocode
let initial_cover = get_cover_state(calibrator_id)?; // before any actuation

close_cover(calibrator_id);
calibrator_on(calibrator_id, brightness);

let result = run_capture_loop(...).await;

// Always clean up, even on error — and restore the cover's initial state
calibrator_off(calibrator_id);
if initial_cover == Open {
    open_cover(calibrator_id);
}

result?; // propagate error after cleanup
```

If cleanup itself fails (e.g., device unreachable), the error is logged
but does not mask the original error. The initial-state read happens
before anything moves, so a failure there aborts the workflow with
nothing to clean up. A cover that started `Closed` — or whose initial
reading was anomalous (`Moving`, `Unknown`, `Error`) — is left closed:
when the starting state cannot be known, covered is the state that
protects the optics.

## Configuration

The plugin reads its configuration from the invocation payload or from
its own config file. Run standalone, `--config` names the flat-plan file
explicitly; when omitted, the path resolves to the platform default
(`~/.config/rusty-photon/calibrator-flats.json` on Linux,
`%PROGRAMDATA%\rusty-photon\calibrator-flats.json` on Windows) via
`rusty-photon-config`. There is no built-in default plan —
the file must exist (`camera_id`, `calibrator_id`, `filters` are
mandatory; `filter_wheel_id` is optional — absent, `null`, or `""` means
the rig has no filter wheel and `set_filter` is never called), so the
packaged systemd unit gates on it with `ConditionPathExists` instead of
crash-looping on a fresh install. Both
`FlatPlan` and `FilterPlan` reject unknown keys at deserialize
(`deny_unknown_fields`), so a typo or a key removed by a schema change
fails loudly at load instead of being silently ignored.

The service's own config file additionally carries a top-level `server`
block for its HTTP endpoint (`/runs`, `/status`, `/health`) and `rp`'s
MCP endpoint:

```json
{
  "server": {
    "port": 11170,
    "bind_address": "0.0.0.0",
    "tls": null,
    "auth": null
  },
  "mcp_server_url": "http://localhost:11115/mcp",
  "service_auth": null,
  "ca_cert": null,
  "camera_id": "main-cam",
  "filter_wheel_id": "main-fw",
  "calibrator_id": "flat-panel",
  "filters": [
    { "name": "Luminance", "count": 20 }
  ]
}
```

A one-shot-color rig omits `filter_wheel_id` and lists a single capture
group (the `name` is a label, not a filter):

```json
{
  "camera_id": "osc-cam",
  "calibrator_id": "flat-panel",
  "filters": [
    { "name": "OSC", "count": 20 }
  ]
}
```

The `server` block is the shared `ServerConfig` from
`crates/rusty-photon-server-config` (see ADR-016): `port`, `bind_address`
(default `0.0.0.0`), and optional `tls`/`auth`. Absent `tls`/`auth` means
plain, unauthenticated HTTP; a plan file without a `server` block keeps
loading (port 11170 on all interfaces). `--port` / `--bind-address`
override `server.port` / `server.bind_address` from the command line.

Turning `server.tls`/`server.auth` on here requires the matching change
on whoever calls this service — the operator or UI posting to `/runs`
(`rp` calls nothing here since mcp-sessionless slice 5).

`service_auth` (optional `{ "username", "password" }`) and `ca_cert`
(optional PEM CA path) apply to calibrator-flats **as a client of rp's
MCP server** — the configured `mcp_server_url` for `/runs` runs, the
payload's URL for legacy invocations; credentials are config-scoped and
never ride a request body. The MCP client is built through the shared `rp-mcp-client`
crate ([ADR-017](../decisions/017-standard-mcp-client-construction.md)),
which enforces the credentials-only-over-verified-HTTPS policy:
`service_auth` without `ca_cert` (or on a non-HTTPS URL) is **not
sent** — the client connects unauthenticated and logs a loud warning.
Doctor `--fix` wires both fields with the D6 observatory credential (see
[doctor.md](doctor.md) §Provisioning).

`calibrator-flats doctor [--config <file>] [--json]` diagnoses this
service's own config read-only without starting it — see
[doctor.md §Per-service doctors](doctor.md). Top-level flags cannot be
combined with the subcommand (the mixed form would silently ignore them).

The plan is the service's own configuration (the `--config` file);
`rp` holds no registration for this service and rejects a `type:
"orchestrator"` entry since mcp-sessionless D11. The `rp` tools it calls
are `capture`, `set_filter`, `get_camera_info`, `compute_image_stats`,
`get_cover_state`, `close_cover`, `open_cover`, `calibrator_on` and
`calibrator_off`:

```json
{
  "config": {
    "camera_id": "main-cam",
    "filter_wheel_id": "main-fw",
    "calibrator_id": "flat-panel",
    "target_adu_fraction": 0.5,
    "tolerance": 0.05,
    "max_iterations": 10,
    "initial_duration": "1s",
    "brightness": null,
    "filters": [
      { "name": "Luminance", "count": 20 },
      { "name": "Red", "count": 20 },
      { "name": "Green", "count": 20 },
      { "name": "Blue", "count": 20 }
    ]
  }
}
```

### Configuration Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `server` | object | `{ "port": 11170 }` | The service's HTTP endpoint (own config file only, not the plugin registration) — the shared `ServerConfig` shape above |
| `service_auth` | object or null | null | HTTP Basic credentials presented to `rp`'s MCP server (own config file only) — the D6 observatory credential |
| `ca_cert` | string or null | null | PEM CA path used to trust a TLS-enabled `rp` (own config file only) |
| `camera_id` | string | required | Camera to use for flat exposures |
| `filter_wheel_id` | string or null | null | Filter wheel to use; absent, `null`, or `""` = no filter wheel (OSC rig): `set_filter` is never called and plan entries are plain capture groups |
| `calibrator_id` | string | required | CoverCalibrator device to control |
| `target_adu_fraction` | float | 0.5 | Target median as fraction of max ADU |
| `tolerance` | float | 0.05 | Acceptable deviation from target (5%) |
| `max_iterations` | int | 10 | Max attempts to find correct exposure time per filter |
| `initial_duration` | humantime string | `"1s"` | Starting exposure time (e.g. `"500ms"`, `"1s"`) |
| `brightness` | int or null | null | Initial calibrator brightness (null = max_brightness); the brightness ladder steps down from here when the exposure search is pinned over-bright |
| `filters` | array | required | List of filters with frame counts |
| `filters[].name` | string | required | Filter name (must match filter wheel config) |
| `filters[].count` | int | required | Number of flat frames to capture for this filter |
| `mcp_server_url` | string or null | null | `rp`'s MCP endpoint, used by every `POST /runs` run; without it `/runs` answers `400` |

## Module Structure

```
services/calibrator-flats/src/
  main.rs            CLI entry point (clap + tracing)
  lib.rs             Public API, ServerBuilder, module declarations
  config.rs          Configuration types (FlatPlan, FilterPlan)
  error.rs           Error types (thiserror)
  routes.rs          Axum router: POST /runs, GET /status, GET /health
  workflow.rs        Flat calibration algorithm (iterative exposure + batch capture)
  mcp_client.rs      MCP client: rp-mcp-client (ADR-017) wrapper to rp's /mcp endpoint
```

## Testing Strategy

Testing follows the conventions in `docs/skills/testing.md`.

### BDD Tests (Cucumber)

BDD tests live in `services/calibrator-flats/tests/` and exercise the
full three-process topology (OmniSim + rp + calibrator-flats) end-to-end:
the run is started with `POST /runs`, drives rp over its MCP tools, and
its outcome is read from the service's own `/status`. The test harness
comes from the `rp-harness` feature
of the `bdd-infra` workspace crate (`bdd_infra::rp_harness`), which
provides the OmniSim singleton, rp launcher, config builder, webhook
receiver, and MCP client.

Current scenarios (`tests/features/flat_calibration.feature`):

- Orchestrator captures flats and reports `complete` on `/status`
- Orchestrator emits an `exposure_complete` event per captured flat

A separate TLS + auth smoke scenario (`tests/features/auth.feature`)
spawns only calibrator-flats itself with `server.tls` and `server.auth`
configured and proves `/health` requires HTTP Basic Auth over HTTPS.

Planned scenarios (not yet implemented):

- Median ADU of captured flats is within tolerance of 50% `max_adu`
- Cleanup on error (calibrator off, cover open)
- Graceful failure when camera or calibrator is unavailable

### Unit Tests

- Configuration deserialization and defaults
- Exposure time adjustment calculation (proportional scaling, clamping,
  divide-by-zero guard)
- MCP client tool call result deserialization

## Future Considerations

- **Brightness optimization**: Instead of only adjusting exposure time,
  the algorithm could also adjust the flat panel brightness to keep
  exposure times in an optimal range (avoiding very short exposures
  where shutter timing becomes significant).
- **Rotator-aware sequencing**: If a rotator is present, flats should be
  taken at the same rotator angle as the corresponding light frames.
- **Per-filter brightness**: Different filters may benefit from different
  panel brightness levels.

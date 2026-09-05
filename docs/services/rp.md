# rp — Main Application Design

## Overview

`rp` is the equipment gateway, event bus, and safety enforcer of Rusty
Photon. It exposes all equipment and services as MCP tools, emits events
that plugins consume, and enforces safety constraints that can override
any operation. It does not contain workflow logic — orchestration is
handled by a separate orchestrator (an MCP client such as
`session-runner`) that starts itself and drives the session by calling
tools on `rp`.

### Tenets

1. **Robustness above all else.** The application survives power failures,
   unresponsive devices, and plugin crashes without losing session progress.
2. **Maximize darkness time.** Every design decision optimizes for shutter-open
   time. Post-capture work runs in parallel with the next exposure.
3. **Automate what is safe to automate.** The planner makes target and filter
   decisions autonomously. Manual intervention is never required during a
   session.
4. **Remote interfaces only.** ASCOM Alpaca for devices, MCP (over HTTP) for
   plugins and UIs. No direct hardware integrations. Ever.
5. **Minimal footprint.** The application runs on Linux, macOS, and Windows, and
   must be efficient enough for a Raspberry Pi 5. Memory and CPU budgets are tight.
6. **Loose coupling via events.** The application emits events; plugins react.
   The application knows as little as possible about what plugins do.
7. **UI is a client, not a component.** The web UI contains zero application
   logic. It renders state and sends commands. Anyone can build an alternative
   UI without changing the application.
8. **MCP is the surface.** Every capability a client drives — UI, plugin, or
   external — is an MCP tool on `rp`'s `/mcp` server; that is the one way to
   command `rp`. HTTP/REST is not a second surface: it carries only what
   cannot ride MCP — raw image bytes (`/api/images/{id}/pixels`), the SSE
   event stream, and plugin completion callbacks — plus config, which must
   stay reachable while conditions are unsafe. A REST endpoint that
   mirrors an MCP tool is a design error, not a feature.

## Architecture

The system is a constellation of independent web services. `rp` is the
equipment gateway at the center — it provides MCP tools, emits events,
and enforces safety. An orchestrator — an MCP client such as
`session-runner` — drives the imaging session by calling tools on `rp`;
`rp` registers, starts and supervises none of them.

```
                       ┌───────────────────┐
                       │     Web UI        │
                       │  (server-rendered │
                       │   HTML or any     │
                       │   framework)      │
                       │  NO app logic     │
                       └────────┬──────────┘
                                │ MCP + SSE
                       ┌────────▼──────────┐
                       │       RP          │
                       │                   │
                       │  MCP Tool Server  │
                       │  Event Bus        │
                       │  Safety Enforcer  │
                       │  Planner          │
                       │  HTTP shim        │
                       └──┬────┬────┬──────┘
                          │    │    │
            ┌─────────────┤    │    ├─────────────┐
            │   Alpaca    │    │    │  Webhooks   │
            ▼             ▼    │    ▼             ▼
       [Camera]      [Mount]   │ [Analyzer]  [Cloud Backup]
       [Focuser]     [FWheel]  │ [Custom]
       [SafetyMon]             │
                               │ MCP (tools/call)
                     ┌─────────┴──────────┐
                     ▼                    ▼
              [Orchestrator]       [Guider Service]
              (session-runner:     (wraps PHD2)
               self-started client)
                     │
                     │ MCP (tools/call)
                     ▼
              [Plate Solver]  [Focus Plugin]  [Centering Plugin]
              (tool providers — compound tools that call back to rp)

            ┌──────────────────────────────────┐
            │          Sentinel                │
            │  Safety monitor (existing)       │
            │  Operation watchdog (new)        │
            │  Corrective actions (new)        │
            │  Subscribes to event bus         │
            └──────────────────────────────────┘
```

### Service Boundaries

Every component is a separate process communicating over HTTP (or JSON-RPC for
PHD2). `rp` is one service among many. Device drivers, plugins,
the guider service, Sentinel, and UIs are all independent processes. This
follows naturally from the Alpaca-only integration tenet — the device drivers
are already separate services.

### Component Categories

`rp` is "batteries included" — it owns the full set of tools and capabilities
that observatory automation routinely needs. Three distinct categories
contribute tools to the MCP catalog, each with its own supervision model and
process boundary:

| Category | What | Examples | Process boundary | Supervised by |
|----------|------|----------|------------------|---------------|
| **Built-in tools** | Rust code running inside `rp`'s own process | Equipment primitives, planner, image analysis (`measure_basic`, HFR, FWHM, eccentricity), V-curve auto-focus, iterative centering | none — same process | Sentinel watches `rp` itself |
| **rp-managed services** | Separate processes that wrap external apps `rp` cannot link against; their tools appear as built-in proxies in the catalog | Guider service (wraps PHD2), plate solver service (wraps ASTAP / astrometry.net) | one process per service | Sentinel restarts on hang/crash |
| **Plugins (extension)** | Separate processes that follow the plugin protocol (event, tool provider): third-party extensions, and first-party capabilities that do not belong in `rp`'s process. | Custom analyzers (ML quality classifiers, wavefront tools), alternative tool providers, custom event consumers. | one process per plugin | `rp` enforces plugin timeouts and the per-tool safety gate; Sentinel may restart configurable plugins |

The category boundary is **process supervision and lifecycle role**, not
authorship. Algorithms that are pure Rust math (auto-focus, centering) live
as built-in tools even though they could in principle be plugins. They become
rp-managed services only when they must wrap an external program (PHD2 the
application, ASTAP the binary) that has its own crash and restart behavior.

Workflow logic stays out of `rp` (design tenet 7), but not as a
plugin: an orchestrator is an ordinary MCP client that `rp` neither
registers nor starts (§ [Orchestration](#orchestration)), so a
different imaging type is a different client — `session-runner` with a
different document, `calibrator-flats`, `polar-align` — and never a
change to the gateway. The plugin mechanism serves **third-party
extensibility**: external developers add tools or event consumers
without forking `rp`. A plugin can be first-party (in the rusty-photon
workspace) or third-party (installed and configured by the operator).
Both follow the same protocol.

From the perspective of an MCP client (an orchestrator, a UI), all
three categories look identical — they are all just tools in the
unified catalog discovered via `tools/list`.

### Port

11115 (configurable)

## Exposure Document

The exposure document is the central data exchange mechanism. Each exposure
produces one document — a sidecar JSON file that lives alongside the FITS file.
The document accumulates data as it flows through the system.

### Core Fields (owned by `rp`)

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "target": {
    "slug": "m31",
    "display_name": "M31",
    "ra_hours": 0.7123,
    "dec_degrees": 41.2689
  },
  "frame_type": "Light",
  "camera_id": "main-camera-1",
  "filter": "Luminance",
  "exposure_time_secs": 300,
  "planned_at": "2026-03-02T01:15:00Z",
  "captured_at": "2026-03-02T01:20:02Z",
  "file_path": "/data/lights/M31/M31_L_5m_001.fits",
  "session_id": "session-2026-03-01",
  "sequence_number": 42,
  "max_adu": 65535,
  "cooler_setpoint_c": -10,
  "sensor_temperature_c": -9.8,
  "optics": {
    "focal_length_mm": 1000.0,
    "pixel_size_x_um": 3.76,
    "pixel_size_y_um": 3.76,
    "sensor_width_px": 9576,
    "sensor_height_px": 6388,
    "pixel_scale_x_arcsec_per_pixel": 0.7756,
    "pixel_scale_y_arcsec_per_pixel": 0.7756,
    "fov_width_deg": 2.0630,
    "fov_height_deg": 1.3762
  }
}
```

**`target` and `frame_type` are landed (Decision 11); `filter`,
`session_id`, `sequence_number`, and `planned_at` remain aspirational —
no code path writes them onto the document yet.** `target` and
`frame_type` are populated only when `capture`'s `frame_type` parameter
was supplied — see [Capture Tool Details](#capture-tool-details) for
the full resolution rules (target-store lookup for `Light`, the
reserved `"dark"`/`"flat"`/`"bias"` slugs for calibration frames
without an explicit `target`). Both fields are omitted (absent, not
`null`) when `frame_type` was omitted — today's flat `<doc_uuid_8>.fits`
capture path.

`max_adu` carries the camera's `MaxADU` capability at the time of
capture. Read once per connection (`connect_camera` stashes it on
`CameraEntry` along with `pixel_size_*` and `camera_*_size` — they are
all invariant physical-sensor properties that cannot change for the
life of the connection) and persisted in the sidecar so the file is
self-describing — the disk-fallback rehydration path in
[Image and Document Cache](#image-and-document-cache) uses it to choose
the `CachedPixels::U16` vs `I32` variant without needing the originating
camera to be connected. `null` (omitted on serialize) when the connect-
time read failed; in that case the cache insert is skipped on every
capture from that camera and the entry serves from disk on demand.

`cooler_setpoint_c` and `sensor_temperature_c` tie each frame to a dark
library: the rung `rp` was regulating at when the frame was captured,
and a best-effort `CCDTemperature` read at capture time. Both are
omitted (absent, not `null`) when unavailable — no ladder configured,
cooling skipped or unreachable, or the temperature read failed. See
[Camera Cooling](#camera-cooling); like `optics`, both are auxiliary
metadata, never gating capture.

`optics` carries the camera + optical-train geometry that consumers
need to interpret the frame without re-deriving it from a plate
solve. Built at capture time from three sources:

1. `focal_length_mm` is operator-supplied on the optical train that
   terminates in this camera (`equipment.optical_trains[].focal_length_mm`
   — see [Optical Trains](#optical-trains)). It captures the light path
   (telescope, reducers, extenders) which has no ASCOM Alpaca property —
   even the optional `Telescope.FocalLength` ignores anything screwed in
   front of the camera.
2. `pixel_size_x_um` / `pixel_size_y_um` come from `cam.pixel_size_x()` /
   `cam.pixel_size_y()` (Alpaca `PixelSizeX` / `PixelSizeY`, microns),
   cached on `CameraEntry` at connect time.
3. `sensor_width_px` / `sensor_height_px` come from `cam.camera_x_size()` /
   `cam.camera_y_size()` (Alpaca `CameraXSize` / `CameraYSize`),
   cached on `CameraEntry` at connect time.

Pixel scale and FOV are derived (`fov_width_deg` corresponds to
`sensor_width_px`; height likewise):

```
pixel_scale_arcsec_per_pixel = 206.265 × pixel_size_um / focal_length_mm
fov_deg                       = pixel_scale_arcsec_per_pixel × sensor_size_px / 3600
```

The block is omitted (serializes as absent, not `null`) when any of
its inputs is unavailable: the camera terminates no optical train (or
its train omits `focal_length_mm`), the camera's `pixel_size_*` read
failed at connect time, or the camera's `camera_*_size` read failed at
connect time. Each missing input is logged at `debug!` (at connect
time for the cached fields; at capture time for the missing focal
length). Capture continues — `optics` is auxiliary metadata, not
gating.

Per-frame variation (binning swaps, focal reducers screwed in
mid-session) is out of scope. The persisted block reflects the
operator-declared static optical train and the camera's reported
sensor geometry at capture time. This is sufficient for `plate_solve`
hint sourcing and for consumers like annotation and mosaic planning.

The `plate_solve` built-in tool (Phase 6c-2 in
`docs/plans/archive/image-evaluation-tools.md`) accepts an explicit
`fov_hint_deg` parameter that callers can populate from
`optics.fov_height_deg` on the document — `fov_height_deg` matches
ASTAP's `-fov` semantics ("image height in degrees"). v1 does **not**
auto-default `fov_hint_deg` from the optics block when the parameter
is omitted; that auto-default is tracked by
[issue #153](https://github.com/rusty-photon/rusty-photon/issues/153)
and lands when the first workflow is blocked without it.

### Plugin Sections (contributed via API)

Plugins write results into named sections. `rp` merges them into the
document and persists the sidecar JSON. Each section is opaque to `rp` — it stores and serves whatever the plugin provides.

```json
{
  "id": "...",
  "...core fields...",
  "sections": {
    "wcs": {
      "ra_center": 10.6848,
      "dec_center": 41.2690,
      "pixel_scale_arcsec": 1.05,
      "rotation_deg": 12.3,
      "solver": "astap-0.9.1",
      "wcs_matrix": {
        "crpix1": 512.0,
        "crpix2": 384.0,
        "cd1_1": -2.91e-4,
        "cd1_2": 1.2e-6,
        "cd2_1": 1.1e-6,
        "cd2_2": 2.91e-4
      }
    },
    "image_analysis": {
      "hfr": 2.3,
      "star_count": 1847,
      "background_mean": 1200,
      "background_stddev": 45
    },
    "guiding_stats": {
      "rms_ra_px": 0.45,
      "rms_dec_px": 0.38,
      "total_rms_px": 0.59
    },
    "weather": {
      "temperature_c": -5.2,
      "humidity_pct": 42,
      "dewpoint_c": -15.1
    },
    "image_stats": {
      "median_adu": 32768,
      "mean_adu": 32450.7,
      "min_adu": 28000,
      "max_adu": 38000,
      "pixel_count": 16777216
    }
  }
}
```

### Persistence

Each capture produces a FITS file and a sidecar JSON document sharing
the same base filename. The base is the first 8 hex characters of the
document's full UUID v4 (`<doc_uuid_8>`):

```
/data/lights/M31/
  550e8400.fits
  550e8400.json    <-- exposure document
```

The optional `session.file_naming_pattern` config, together with
`session.directory_pattern`, is a **round-trippable** filename
template (P1 of
[planetarium-target-import.md](../plans/planetarium-target-import.md)):
`rp` both renders filenames from capture context and parses them back
to recover `(target, filter, binning, exposure_duration)` for goal-progress
derivation (see [Target Store](#target-store)). The full contract —
per-token typed shapes and the compiled-anchored-regex requirement —
lives in
[`rp-targets.md` § File-naming template](../crates/rp-targets.md#file-naming-template-render-and-parse).
Both patterns are parsed and validated at config-load time — an
unknown token or an ambiguous adjacent-token pair fails startup, not a
session; `file_naming_pattern` additionally requires the quota tokens
(`{target}`/`{filter}`/`{binning}`/`{exposure_duration}`), a stricter
contract than `directory_pattern` (whose documented default,
`"{target}/{night_date}/{frame_type}"`, has none of them). Each compiles
into a reusable render/parse engine (`CompiledTemplate`, backed by the
`regex` crate: each token's shape becomes a named capture group in one
combined anchored regex, so `parse` is never a naive `split('_')`).

**The UUID-8 suffix is not a token.** Every frame's filename ends in
`_<uuid8>` — the first 8 hex characters of its exposure document's UUID
— appended by `rp` *after* the rendered pattern, so the on-disk name is
`<rendered file_naming_pattern>_<uuid8>.fits` (or bare `<uuid8>.fits`
with no pattern configured). That suffix is the reverse-lookup key the
disk-fallback resolver depends on ([Document
Resolution](#document-resolution)), which is why the pattern cannot opt
out of it, move it, or substitute `{frame_number}` for it: a pattern
containing `{uuid8}` is rejected at load with a message saying the
suffix is automatic. The compiled file template renders and parses the
suffix itself, so `parse(render(x)) == x` still holds and the progress
scan sees the document id of every frame it counts. Per-frame
uniqueness therefore never depends on the operator's pattern —
`{frame_number}` is optional, a readable sequence number rather than a
uniqueness guarantee.

**Path shape is validated too**, because `capture` joins the rendered
directory onto `data_directory` while the progress scan walks
`data_directory` to the pattern's component depth — the two have to
agree on what a component is. `directory_pattern` must therefore be a
canonical relative path: no leading `/` (an absolute path makes
`PathBuf::join` discard the base, writing frames outside
`data_directory` altogether), no trailing or repeated `/` (the
filesystem collapses the empty component when `capture` creates the
directory, but the scan would still walk a level too deep and match
nothing), and no `.` or `..` component. `file_naming_pattern` may not
contain `/` at all: it names one file inside that directory, and
nesting it deeper puts the frame where the scan does not look. Both
patterns additionally reject `\` and `:` on **every** host, not just
Windows: a config that loads on Linux has to mean the same thing on
Windows, where `\` separates paths (the scan would count one component
and walk the wrong depth) and `C:` is drive-absolute (`PathBuf::join`
discards the base, so frames escape `data_directory`). Neither
character is legal in a Windows filename regardless.

Those are instances of one rule rather than a list: **`capture` must be
able to create exactly the name the scan will look for, on every
supported platform.** So the rest of Windows' illegal filename set
(`<>"|?*` and control characters) is rejected too, as is a component
with a trailing `.` or space — Windows strips those, so `capture` would
create `night.` as `night` while the scan kept looking for `night.`. No
token's shape can introduce any of them, so checking the pattern text is
exact: every occurrence came from a literal someone typed. Each is
rejected at load rather than quietly canonicalized — rewriting what the
operator wrote is how the writer and the scanner drift apart in the
first place, and the symptom otherwise is a night of frames that
silently count for nothing.

**An empty pattern is malformed, not unset**, and both fields say so
alike. Absent (or `null`) is how each is turned off — `directory_pattern`
falls back to its documented default, `file_naming_pattern` falls back to
flat `<uuid8>.fits` capture — while `""` is a pattern with nothing in it,
which for a directory means a path with no components the progress scan
could never match. The two are different states, so only the spelling
that means "unset" is read that way, and the load error names it. This
matters most through the config UI, where clearing a text box is the
natural gesture for turning a field off: the BFF sends `null` for a
cleared optional field precisely so that gesture reaches rp as the state
the operator intended (ui-htmx.md § Schema-driven config form).

**Landed (Decision 11).** `capture`'s `target`/`frame_type` parameters
(§ Capture Tool Details) feed `render`, and rendering replaces the flat
`<doc_uuid_8>.fits` whenever `frame_type` is supplied: the rendered
directory base is prefixed before the filename base, which is in turn
prefixed before the UUID-8 suffix, e.g.
`m31/2026-07-22/Light/m31_L_1x1_0001_5m_fpos_2_-10C_550e8400.fits`.
The disk-fallback resolver walks `data_directory` to
`directory_pattern`'s component depth — the same depth the progress
scan walks — checking every level on the way down, so a templated frame
three directories deep and a flat `<uuid8>.fits` from a `frame_type`-less
capture are both reachable by id after eviction or restart.
`directory_pattern` defaults to `"{target}/{night_date}/{frame_type}"`
when unset but `file_naming_pattern` is configured — only the file
pattern needs explicit configuration to opt in. The same `parse` runs on both sides of
the round trip: `capture` calls it to compute each new frame's
`{frame_number}`, and the progress scan calls it to attribute frames on
disk back to a target and goal (§ Progress derivation).

The full UUID is the canonical document identifier — used by the API,
the FITS header, and the sidecar's `id` field. The 8-char suffix
exists only on disk, as the reverse-lookup key for finding a
document's files when given its id. See
[Image and Document Cache](#image-and-document-cache) and
[Document Resolution](#document-resolution) for the rules.

The FITS file's primary HDU header carries `DOC_ID = '<full-uuid>'`,
making each FITS self-describing for lineage, downstream tools, and
disambiguation when multiple files in the data directory happen to
share an 8-char suffix. The sidecar's `id` field carries the same
full UUID as a fallback authority.

Both the FITS file and the sidecar JSON are written atomically:
staged to a sibling temp file, fsynced, renamed into place, parent
directory fsynced. A crash mid-write cannot leave a torn file. The
document is updated as plugins and tools contribute sections; each
section update re-serializes the full sidecar atomically.

The disk pair is durable beyond `rp`'s runtime. After eviction from
the in-memory cache (or after `rp` restart), a document remains
accessible via the API as long as its FITS+sidecar pair sits in
`<data_directory>`. The contract is "live as long as the file is on
disk", not "live as long as `rp` is up".

## Event System

`rp` emits events. Plugins and services subscribe via webhooks.
The application does not know or care what subscribers do with events.

Every blocking operation emits a lifecycle *triple* — a `*_started`
event at entry and a matching `*_complete` or `*_failed` at exit —
correlated by a shared `operation_id` and wrapped in the uniform
[Event Envelope](#event-envelope) below. (`sync_mount` is instant, so it
emits only `_complete` / `_failed`, with no `_started`.) Point events
(e.g. `filter_switch`, `safety_changed`) carry no `operation_id`.

### Events

| Event | Payload | When |
|-------|---------|------|
| `exposure_started` | camera_id, duration | Exposure begins |
| `exposure_complete` | document_id, file_path | Readout finished, document persisted |
| `exposure_failed` | error | Exposure failed (start error, camera error state, readout timeout, or FITS write) |
| `slew_started` | ra, dec | Mount begins slew |
| `slew_complete` | ra, dec, actual_ra, actual_dec | Mount reports slew done |
| `slew_failed` | error | Slew failed or timed out (best-effort abort issued) |
| `park_started` | — | Mount begins parking |
| `park_complete` | — | Mount reports `AtPark` |
| `park_failed` | error | Park failed or timed out (no auto-abort, per the park contract) |
| `unpark_started` | — | Mount begins unpark |
| `unpark_complete` | — | Mount unparked |
| `unpark_failed` | error | Unpark failed |
| `sync_mount_complete` | ra, dec | Mount sync applied (instant — no `_started`) |
| `sync_mount_failed` | error | Mount sync failed |
| `move_focuser_started` | focuser_id, position | Focuser begins move to the target position |
| `move_focuser_complete` | focuser_id, position | Focuser idle at the read-back position |
| `move_focuser_failed` | error | Focuser move failed or timed out |
| `move_rotator_started` | rotator_id, angle, guiding_paused | Rotator move begins (before the ladder's pause, which is part of the operation); `guiding_paused` says whether the rotate-while-guiding ladder engaged for this move |
| `move_rotator_complete` | rotator_id, angle, mechanical_angle, moved_trains, guiding_ladder | Rotator idle at the read-back angle; `moved_trains` lists the trains containing it, `guiding_ladder` the ladder outcome (`null` when it did not engage) |
| `move_rotator_failed` | error | Rotator move failed or timed out (guiding re-selected/resumed best-effort when the ladder was engaged) |
| `plate_solve_started` | document_id, image_path, use_mount_hints | Plate solve begins |
| `plate_solve_complete` | ra_center, dec_center, pixel_scale_arcsec, rotation_deg, solver | Plate solve succeeded |
| `plate_solve_failed` | error | Plate solve failed |
| `centering_started` | camera_id, ra, dec, tolerance_arcsec, max_attempts | Plate-solve + correct loop begins |
| `centering_iteration` | camera_id, document_id, residual_arcsec, solved_ra, solved_dec, action | One centering iteration completed |
| `centering_complete` | camera_id, final_error_arcsec, attempts, final_ra, final_dec | Centering converged |
| `centering_failed` | error | Centering failed |
| `focus_started` | camera_id, focuser_id, position, temperature | Auto-focus begins |
| `focus_complete` | camera_id, focuser_id, position, hfr, samples_used | Auto-focus result |
| `focus_failed` | error | Auto-focus failed |
| `refocus_started` | train_id, reason, steps, guiding_paused | Dependency-ordered refocus begins; `steps` lists `{focuser_id, train_id}` in run order, `guiding_paused` says whether rp pauses guide corrections for the sequence |
| `refocus_complete` | train_id, steps | Every AF step done (guiding resumed if it was paused); `steps` carries per-step `{focuser_id, train_id, camera_id, best_position, best_hfr, samples_used}` |
| `refocus_failed` | error | A step failed, the pause/resume handshake failed, or the expansion was invalid |
| `guide_started` | recalibrate, settle_pixels, settle_time, settle_timeout | Guiding loop starting; carries the settle deadline (`max_duration_ms` = settle_timeout + the service's 10 s backstop grace) when a settle timeout is resolved |
| `guide_settled` | rms_ra_px, rms_dec_px, total_rms_px, sample_count | Post-start settle complete |
| `guide_failed` | error | Guiding start or settle failed |
| `guide_stopped` | reason (`requested` \| `safety`) | Guiding stopped (point event) |
| `guide_rotator_unmodeled` | rotator_id, train_id | `start_guiding` settled with a rotator-coupled guide camera but PHD2 reports no connected rotator (point event — see [Guider Service](#guider-service)) |
| `guide_focus_degraded` | train_id, baseline_hfd, current_hfd, window | The [Guide Focus Watch](#guide-focus-watch)'s trailing HFD median exceeded `baseline × degrade_ratio` (point event; held by `cooldown`). `train_id` names the guiding train (null when the watch runs without one), so a workflow trigger can address the guide-only sweep without knowing the rig |
| `guide_focus_escalation` | train_id, baseline_hfd, current_hfd | A degradation episode is still degraded `escalation_deadline` after `guide_focus_degraded` — the full `refocus_train` sequence is indicated (point event; once per episode). `train_id` names the guiding train (null when the watch runs without one, same as `guide_focus_degraded`) |
| `dither_started` | pixels, ra_only, settle_pixels, settle_time, settle_timeout | Dither command sent; deadline as on `guide_started` |
| `dither_settled` | rms_ra_px, rms_dec_px, total_rms_px, sample_count | Post-dither settle complete |
| `dither_failed` | error | Dither or its settle failed |
| `mount_motion_pending` | operation (`slew` \| `dither`) | A mount motion is queued behind the [mount motion gate](#mount-motion-gate) — in-flight imaging-train exposures (or an earlier queued motion) must finish first. Point event; the motion's own `*_started` triple follows once the gate is acquired |
| `safety_changed` | monitor, new_state | SafetyMonitor transition |
| `equipment_changed` | kind, device, connected | A device session was re-established (`connected: true`, emitted on every successful re-establishment) or lost (`connected: false`, once per transition) by the reconnect supervisor (§ [Device Session Recovery](#device-session-recovery)). `kind` is the device type (`camera`, `mount`, …); `device` is the config id, `null` for the singular mount |
| `provider_changed` | provider, connected | A tool provider's MCP session was re-established (`connected: true`) by the reconnect supervisor's provider lane, or lost (`connected: false`, once per transition — on the first failed proxied call or a failed health check). `provider` is the registration's `name` (§ [Plugin-Provided Tools](#plugin-provided-tools)) |
| `temperature_changed` | sensor, value | Significant temperature change |
| `cooler_stabilized` | camera_id, target_c, floor_c (only when a floor was measured), power_pct (only when readable) | Cooldown selected and stabilized at a dark-library rung (§ Camera Cooling) |
| `cooler_unreachable` | camera_id, floor_c, warmest_target_c | No configured rung reachable tonight; cooler switched off, session proceeds uncooled |
| `cooler_warmup_started` | camera_id, from_c, target_c | Warm-up ramp begins at session end |
| `cooler_warmup_complete` | camera_id | Warm-up ramp finished, cooler off |
| `meridian_flip_started` | hour_angle | Flip initiated |
| `meridian_flip_complete` | — | Flip and re-center done |
| `target_switch` | old_target, new_target | Planner decided to switch targets |
| `filter_switch` | camera_id, old_filter, new_filter | Filter change on a camera |
| `frame_rejected` | document_id, plugin, reason | Immediate correction rejected a frame |
| `plugin_timeout` | plugin, event_id | Plugin did not respond within `max_duration` |
| `document_updated` | document_id, section_name | Plugin contributed a section |
| `document_persistence_failed` | document_id, file_path, error | Sidecar write failed during capture. The FITS file is on disk but the cache is not populated and no sidecar exists; `document_id`-keyed lookups return 404 (disk fallback requires the sidecar). Recover by reading the FITS via `file_path` from the payload. See [Capture Tool Details](#capture-tool-details). |

### Event Envelope

Every emitted event is wrapped in a uniform envelope. The envelope is
**additive** over the historical webhook body: `event_id`, `event`,
`timestamp`, and `payload` keep their exact meaning and contents, so
existing webhook plugins are unaffected. New fields are carried alongside
and absent optional fields are omitted from the JSON.

```json
{
  "event_id": "f3a8b9c0-1d4e-4a2b-8f3a-2c7d9e1f4b6a",
  "event_seq": 1247,
  "operation_id": "0bbc7e54-c2c2-4e3b-9a8d-7f43a3a8b2f1",
  "event": "slew_started",
  "timestamp": "2026-05-19T20:14:33Z",
  "started_at": "2026-05-19T20:14:33.412Z",
  "predicted_duration_ms": 21000,
  "max_duration_ms": 63000,
  "payload": { "ra": 12.0, "dec": -30.0 }
}
```

| Field | Meaning |
|-------|---------|
| `event_id` | Per-emission UUID. Unchanged; the routing key for the plugin completion contract (`POST /api/plugins/{event_id}/complete`). |
| `event_seq` | Monotonically increasing per-emission counter. Total order across all events; the SSE `id` (and `Last-Event-ID` replay key) for the [Real-Time Stream](#real-time-stream). |
| `operation_id` | Correlation key shared by an operation's `*_started`, `*_complete`, and `*_failed` events. Omitted for point events (e.g. `filter_switch`, `safety_changed`). |
| `event` | The event-type string, e.g. `"slew_started"`. |
| `timestamp` | ISO-8601 emission time. Unchanged historical format. |
| `started_at` / `ended_at` | RFC-3339 (millisecond) operation start / end. `started_at` is on the `*_started`/`*_complete`/`*_failed` triple; `ended_at` only on `*_complete`/`*_failed`. |
| `elapsed_ms` | Wall-clock operation duration, on `*_complete`/`*_failed`. |
| `predicted_duration_ms` / `max_duration_ms` | The operation's expected duration and hard-ceiling deadline, in integer milliseconds (a boundary serialization of an internal `Duration`). Populated (Phase 2) on: `slew_started` — `predicted = great-circle distance / mount.slew_rate_arcsec_per_sec + settle`, `max = max(predicted × 3, MIN_SLEW_DEADLINE = 30 s)`; `park_started` — `predicted = 180° / mount.slew_rate_arcsec_per_sec + settle` (worst-case traverse; rp can't read the park position via Alpaca), `max = max(predicted × 2, MIN_PARK_DEADLINE = 60 s)`; `move_focuser_started` — `predicted = \|target − current\| / focuser.steps_per_sec`, `max = max(predicted × 2, MIN_FOCUSER_DEADLINE = 5 s)`; `exposure_started` — `predicted = duration + camera.readout_time_estimate` (default 15 s when unset), `max = predicted + 30 s` readout headroom (advisory only — rp does not enforce it; the camera driver owns the exposure, and rp keeps a separate, more generous internal readout backstop); `centering_started` — outer-loop only (advisory; per-iteration slews/captures carry their own deadlines): `per_iter = duration + centering.solve_time_estimate (default 30 s) + centering.slew_overhead_estimate (default 10 s)`, `predicted = per_iter` (single-pass convergence), `max = max_attempts × per_iter`. Omitted for operations not yet converted to predictive deadlines. |
| `payload` | Operation detail. For `*_started`, the inputs; for `*_complete`/`*_failed`, the outcome (or `{"error": "..."}` on failure). |

A blocking operation emits a **triple** — a `*_started` envelope at the
entry point and a `*_complete` or `*_failed` envelope at the end, all
sharing one `operation_id`. (`sync_mount`, being instant per ASCOM, emits
only `*_complete` / `*_failed`.) See
[`docs/plans/archive/predictive-deadlines-and-watchdog.md`](../plans/archive/predictive-deadlines-and-watchdog.md)
for the deadline-monitoring design this envelope feeds.

### Delivery: Webhooks

Plugins register a callback URL and subscribed events in the configuration.
`rp` POSTs events to each registered URL. All plugins use the same
asynchronous request-response pattern.

**A `type: "event"` registration must be deliverable.** `name`,
`webhook_url`, and a `subscribes_to` list holding at least one
event-name string are all required. Each is checked at config load with
the offending entry named (`plugins.0.subscribes_to`), and `rp` refuses
to start when one is missing or mistyped. Accepting such a registration
as inert is the more dangerous reading: a `subscribe_to` typo would leave
a plugin that looks registered, logs nothing, and receives nothing for
the whole night. There is deliberately no "registered but idle" state —
to stop delivering to a plugin, remove its registration.

**A subscriber may serve TLS and require authentication.** A
`webhook_url` may be `https://` when the plugin's own service is
TLS-enabled; `rp` verifies that certificate against the top-level
`ca_cert`, the same single observatory-CA setting every other `rp` client
uses (see [Configuration](#configuration)). The optional `auth` block is
the plaintext credential `rp` presents as HTTP Basic on every delivery,
mirroring `plate_solver.auth` and `equipment.mount.guiding.auth`; omit
it for a plugin that does not challenge. Both are validated at config
load, so startup, `PUT /api/config` and `rp doctor` refuse the same
file: `webhook_url` must be an `http://` or `https://` URL (the rule
`server.advertised_url` follows) carrying no embedded credentials —
`https://user:pass@host/…` is rejected, because `rp` logs the URL on
every delivery and the `auth` block beside it, applied per request and
marked sensitive, is the supported way to authenticate — and `auth`,
when present, must be a complete `{username, password}` pair. Either
one malformed is rejected naming the offending entry
(`plugins.0.webhook_url`, `plugins.0.auth`) rather than read as "no
credential" and answered 401 on the first delivery of the night. A
registration is otherwise opaque to `rp` — unknown keys are the plugin
author's business — and `rp` reads neither field on a registration it
does not dial, so a tool-provider entry carrying its own
differently-shaped `auth` key is left alone. A `ca_cert` the delivery
client cannot be built from fails startup the same way. Doctor reports
the join between `plugins[].webhook_url` and the plugin service's own
`server.tls`/`server.auth` as `joins.client-transport` /
`joins.client-auth`, and `doctor --fix` wires both (see
[doctor.md § Client-target joins](doctor.md#client-target-joins-607)).
Any number of event subscribers may be registered, because `rp`
delivers each event to all of them. Getting a registration wrong is
quiet: delivery is fire-and-forget with no retry, so the event is
simply lost and the night continues. A plugin that *answers* with a
non-success status — a 401 from
a wrong credential, a 500 from a plugin bug — is logged at `warn!` with
that status, because that line is the only signal an operator gets that a
subscriber is silently doing nothing; a plugin that cannot be reached at
all stays at `debug!`, since an unreachable subscriber is the ordinary
case for a plugin that is simply not running.
`rp` reads a callback URL and `auth` only on the registrations it
dials — the event kind — so a tool provider's own
differently-shaped `auth` key is left alone. One client, built once at
startup, serves every subscriber; a `ca_cert` it cannot be built from
fails startup, and with no subscriber registered no client is built at
all. Doctor reports the join between `plugins[].webhook_url` and the
plugin service's own `server.tls`/`server.auth` as
`joins.client-transport` / `joins.client-auth`, and `doctor --fix` wires
both (see
[doctor.md § Client-target joins](doctor.md#client-target-joins-607)).

#### Request

```
POST <plugin_webhook_url>
Content-Type: application/json

{
  "event_id": "evt-550e8400-e29b-41d4",
  "event": "exposure_complete",
  "timestamp": "2026-03-02T01:25:02Z",
  "payload": {
    "document": { ... },
    "file_path": "/data/lights/M31/M31_L_5m_001.fits"
  }
}
```

#### Step 1: Acknowledgment (immediate HTTP response)

The plugin responds immediately to the webhook HTTP request with an
acknowledgment declaring how long it expects to take:

```json
{
  "estimated_duration": "20s",
  "max_duration": "30s"
}
```

- `estimated_duration`: humantime string for how long the plugin expects
  processing to take. The planner uses this for scheduling decisions.
  Provided dynamically per invocation — a plate solve on a wide-field
  image may differ from a narrow-field one.
- `max_duration`: hard timeout. If the plugin doesn't complete within
  this time, `rp` proceeds and emits a warning.

`rp` records the durations and continues with the orchestration. The next
exposure can start immediately after `exposure_complete` — the plugin
processes in parallel.

#### Step 2: Completion (callback POST to `rp`)

When the plugin finishes processing, it POSTs a completion to `rp`:

```
POST /api/plugins/{event_id}/complete
Content-Type: application/json

{
  "status": "complete"
}
```

Or, to request a corrective action:

```json
{
  "status": "complete",
  "correction": {
    "action": "focus",
    "reason": "HFR degraded from 2.3 to 4.8 — likely focus drift",
    "urgency": "immediate"
  }
}
```

- `correction` (optional): requests that the orchestrator perform a
  corrective action (see Corrections below).
  - `action`: the corrective action to take (e.g., `"focus"`,
    `"center"`). Must be a recognized action name.
  - `reason`: human-readable explanation, logged and included in events.
  - `urgency`: either `"immediate"` (abort in-flight operations, reject
    the frame) or `"after_current"` (queue until the current operation
    completes naturally, frame counts normally).

#### Barriers

A plugin can optionally declare **barrier gates** — MCP tools that must
not proceed until the plugin has posted its completion for the most
recent webhook. This tells `rp`: "if you haven't heard back from me yet,
block these tools until you have."

```json
{
  "name": "image-analyzer",
  "webhook_url": "http://localhost:11140/webhook",
  "subscribes_to": ["exposure_complete"],
  "barrier_gates": ["slew", "set_filter"]
}
```

When the orchestrator calls a gated tool, `rp` checks whether any
barrier plugin still has an outstanding (uncompleted) webhook. If so,
`rp` blocks the tool call — up to `max_duration` from the
acknowledgment — before executing. All outstanding plugins are waited on
in parallel.

A plugin with no `barrier_gates` (or an empty list) is never waited on.
Its completion is still processed when it arrives, but `rp` never blocks
on it.

If a barrier plugin completes with a correction while a tool call is
blocked, the gated tool returns the correction to the orchestrator
instead of executing (see Corrections below).

#### Corrections

A plugin can request that the orchestrator perform a corrective action
by including a `correction` in its completion. Corrections have two
urgency levels that determine how `rp` delivers them to the
orchestrator:

**`immediate`** — the current frame is unusable. `rp` aborts any
in-flight operation (e.g., aborts the active camera exposure), returns
the correction to the orchestrator in the aborted tool call's result,
and rejects the frame:

```json
{
  "status": "aborted",
  "correction": {
    "action": "focus",
    "reason": "HFR 4.8, frame unusable",
    "source": "image-analyzer"
  }
}
```

**`after_current`** — the current frame is still usable, but a
corrective action should happen before the next exposure. `rp` queues
the correction and surfaces it in the result of the current in-flight
tool call when it completes naturally:

```json
{
  "image_path": "/data/lights/M31/M31_L_5m_004.fits",
  "document_id": "doc-043",
  "pending_correction": {
    "action": "focus",
    "reason": "HFR 3.0, trending worse",
    "source": "image-analyzer"
  }
}
```

In both cases the orchestrator decides **what to do** with the
correction. `rp` controls **when** the orchestrator hears about it.

**Conflict resolution:** when multiple plugins request corrections,
the most disruptive action wins. If one plugin requests refocus and
another requests recenter, recenter wins because it includes refocusing.

**Frame rejection:** an `immediate` correction implicitly rejects the
frame that triggered the event. `rp`:

1. Does not count the rejected frame toward the exposure goal.
2. Marks the exposure document with the rejection reason.
3. Emits a `frame_rejected` event.

An `after_current` correction does not reject the frame. The current
exposure counts normally.

**Barrier interaction:** when a barrier plugin completes with a
correction while a gated tool call is blocked, `rp` returns the
correction to the orchestrator instead of executing the gated tool.
The orchestrator sees the correction and acts accordingly (e.g.,
refocuses instead of slewing to a new target).

#### Timeout Behavior

When `max_duration` (from the acknowledgment) expires without a
completion:

1. `rp` proceeds as if the plugin completed with `"complete"` and no
   correction.
2. If a tool call was blocked on this barrier, it unblocks and executes
   normally.
3. A `plugin_timeout` warning event is emitted.
4. The timeout is logged.

Webhook delivery failures (connection refused, HTTP errors) are treated
as immediate completion with no correction. Plugins are responsible for
their own reliability.

#### Example: Image Analyzer Flow

Setup: 5 exposures on the same target, 5m each, analysis takes 20s.

```
Exposure 3 completes
  → rp POSTs exposure_complete to analyzer
  → analyzer responds immediately:
      {"estimated_duration": "20s", "max_duration": "30s"}
  → rp records outstanding barrier, starts exposure 4 (not gated)

  Case A — frame OK, no target switch pending:
    → analyzer POSTs completion: {"status": "complete"}
    → rp notes completion, clears barrier
    → capture continues normally

  Case B — frame bad (immediate), exposure 4 in-flight:
    → analyzer POSTs completion:
        {"status": "complete", "correction": {"action": "focus",
         "reason": "HFR 4.8", "urgency": "immediate"}}
    → rp aborts exposure 4, returns capture with:
        {"status": "aborted", "correction": {"action": "focus", ...}}
    → orchestrator refocuses, resumes capture

  Case C — frame marginal (after_current), exposure 4 in-flight:
    → analyzer POSTs completion:
        {"status": "complete", "correction": {"action": "focus",
         "reason": "HFR 3.0, trending", "urgency": "after_current"}}
    → rp queues correction, exposure 4 continues
    → exposure 4 completes, capture returns with:
        {"image_path": "...", "pending_correction": {"action": "focus", ...}}
    → orchestrator refocuses before starting exposure 5

  Case D — frame bad, slew pending (barrier in action):
    → orchestrator calls slew → rp blocks (outstanding barrier)
    → analyzer POSTs completion with immediate correction
    → rp returns slew with correction instead of executing:
        {"status": "blocked_by_correction",
         "correction": {"action": "focus", ...}}
    → orchestrator refocuses, stays on current target
```

### Plugin Section Updates

After processing an event, plugins POST their results back to `rp`:

```
POST /api/documents/{document_id}/sections
Content-Type: application/json

{
  "section_name": "wcs",
  "data": {
    "ra_center": 10.6848,
    "dec_center": 41.2690,
    "pixel_scale_arcsec": 1.05,
    "rotation_deg": 12.3,
    "solver": "astap-0.9.1"
  }
}
```

`rp` merges the section into the document and persists the updated
sidecar JSON.

## Action System

The action system complements the event system. Where events flow outward
from `rp` to plugins (notifications), actions flow inward from plugins to
`rp` (requests). Actions are the primitives that plugins use to control
equipment and trigger computations through `rp`.

The action system uses the
[Model Context Protocol (MCP)](https://modelcontextprotocol.io/) as its
wire protocol. `rp` runs an MCP server that exposes all available actions
as **MCP tools**. Workflow plugins connect as MCP clients to discover and
call tools.

MCP provides:

- **Tool discovery** — `tools/list` returns all available tools with
  JSON Schema parameter definitions.
- **Typed invocation** — `tools/call` with schema-validated parameters
  and structured results.
- **Formal schemas** — every tool's parameters and return types are
  described by JSON Schema, derived from Rust types at compile time
  (via `#[tool]` + `JsonSchema` derives in the `rmcp` crate).
- **Language-agnostic** — plugins can be written in any language with an
  MCP client library (Rust, Python, TypeScript, Go, etc.).

`rp` never exposes raw device access. Every tool validates parameters,
enforces safety constraints, and tracks state before touching hardware.

### MCP Server

`rp` runs a single MCP server using the streamable HTTP transport. This
server exposes all available tools — both built-in and aggregated from
plugin providers (see Plugin-Provided Tools below).

The server endpoint is configurable (default: `http://localhost:11115/mcp`).
Orchestrators and every other client connect to this endpoint as MCP
clients; `rp` hands the URL to nobody — each client is configured with
it (`session-runner`'s `mcp_server_url`, `ui-htmx`'s `rp.url`, and so
on). The name a client should use follows from the listener: the scheme
is `https` when `server.tls` is set (else `http`), the port is the bound
listener port, and the host is the bind address — or, for a wildcard
bind (`0.0.0.0` / `::`), the **system hostname**, which is reachable
from the machine itself and from any LAN host that resolves it, and is
a DNS SAN on every doctor-provisioned certificate. `server.advertised_url`
names the reachable URL when it is not derivable — NAT, a reverse
proxy, or an ACME DNS name — and its effect is on the allowlist below.

The MCP transport validates the inbound `Host` header — the standard
defence against DNS rebinding a browser onto a locally bound server —
and answers `403 Forbidden` to any authority not on its allowlist. The
allowlist is derived from the same facts, so every name a client can
reasonably be configured with is one `rp` accepts:

- `localhost`, `127.0.0.1` and `::1` — always.
- The **system hostname**, whenever it can be determined. This is the
  host a wildcard bind advertises.
- The authority of `server.advertised_url` when configured, port
  included when the URL names one.
- The literal `server.bind_address` for an explicit (non-wildcard)
  bind — the one address that listener answers on.
- Every non-loopback address of a local network interface (IPv4 and
  IPv6) for a wildcard bind — the addresses that listener actually
  answers on, so an orchestrator holding only a LAN address connects.
  Enumeration failure is logged and non-fatal; the entries above still
  stand. The packaged systemd unit grants `AF_NETLINK` in
  `RestrictAddressFamilies=` for this call — glibc's `getifaddrs(3)`
  opens a netlink socket, and a sandbox without the grant fails the
  enumeration with `EAFNOSUPPORT`, silently dropping the LAN-address
  entries.

Entries carry no port unless `server.advertised_url` supplied one, and
a port-less entry matches its name on any port. A reachable name that
is derivable from none of the above — an mDNS alias, a reverse-proxy
hostname, an ACME DNS name — is admitted by pointing
`server.advertised_url` at it, which both advertises and allows it.

The endpoint sits behind the same server-wide `server.tls` /
`server.auth` as every other route — there is no unauthenticated MCP
carve-out. First-party MCP clients are built through the shared
`rp-mcp-client` crate, which presents the D6 observatory credential over
verified TLS only
([ADR-017](../decisions/017-standard-mcp-client-construction.md)).

**Protocol posture.** `rp` speaks MCP 2026-07-28 and serves every
request **statelessly**
([ADR-021](../decisions/021-session-less-mcp-and-the-safety-contract.md)):

- There are no sessions. No response ever carries an `Mcp-Session-Id`
  header, and no request is expected to. A client bootstraps with
  `server/discover` (which returns the versions `rp` supports) and
  then sends self-contained requests: the `MCP-Protocol-Version`
  header, the `Mcp-Method` header naming the JSON-RPC method (plus
  `Mcp-Name` naming the tool on a `tools/call`), and a `_meta` block
  carrying `io.modelcontextprotocol/protocolVersion` and
  `io.modelcontextprotocol/clientCapabilities` (the client's
  `clientInfo` is welcome but not required). A request whose `_meta`
  disagrees with its header is rejected with the transport's
  header-mismatch error before dispatch.
- A client speaking an older revision (`2025-03-26` … `2025-11-25`)
  still works: its `initialize` is answered — with no session id — and
  its later requests are served statelessly like everyone else's.
  Nothing in `rp` depends on the handshake having happened.
- Nothing is required per request beyond the protocol signals above:
  `stateless_protocol_metadata_required` stays off, so a request that
  omits the 2026-07-28 `_meta` fields is served as a legacy request
  rather than rejected (mcp-sessionless plan, O4).
- A `tools/call` is answered as plain JSON unless the body emits a
  `notifications/progress` first, in which case the response is an SSE
  stream carrying the notifications and then the result. There is no
  keep-alive to satisfy between calls: a client may stay idle for any
  length of time and its next call is served like its first.
- `tools/list` is deterministic for the life of the process — the
  catalog is built once at startup (§ [Tool Catalog](#tool-catalog)),
  provider tools included — so a 2026-07-28 client is told it may
  cache the listing (`ttlMs` 60 000, `cacheScope: "private"`). The
  bound only limits how long a stale listing survives an `rp` restart
  that changed the provider set.
- Cancellation is per request, not per session: the in-flight registry
  (§ [In-Flight Tool Calls](#in-flight-tool-calls)) cancels a body when
  the caller's HTTP request goes away before its response, or when the
  caller sends `notifications/cancelled`.

First-party clients pin `2026-07-28` and bootstrap with
`server/discover`; "rp unreachable" surfaces at connect as a failed
discovery. There is no transparent session re-establishment because
there is no session to re-establish; a consumer that loses `rp`
reconnects on its own terms, and one that is refused for safety
(§ Safety) waits for safe conditions rather than reconnecting.

### Tool Catalog

The catalog is built at startup from three sources, all of which appear
identical to MCP clients:

1. **Built-in tools** — implemented directly in `rp` (hardware primitives,
   image analysis, planner, V-curve auto-focus, iterative centering).
2. **rp-managed service tools** — built-in tool surface that proxies to a
   separate process `rp` supervises (guider, plate solver). The MCP tool
   itself lives in `rp`; the wrapped logic runs in the supervised service.
3. **Third-party plugin tools** — aggregated from plugins running their own
   MCP servers. Discovered at startup via `tools/list` and proxied through
   `rp`'s server (§ [Plugin-Provided Tools](#plugin-provided-tools)). A
   name a provider shares with a built-in or with another provider fails
   startup: the catalog has no precedence rule to guess at.

Workflow plugins discover available tools via the standard MCP
`tools/list` call. Each tool includes its JSON Schema, so plugins know
the exact parameter types and return structure.

Every tool also has a **safety class** — gated or ungated (§ Safety →
[In-Flight Tool Calls](#in-flight-tool-calls)). The hardware, guider,
safety and compound tables below carry it as a column; every compute,
planner, target-store and schema tool is ungated. The class decides
only what happens while conditions are unsafe: a gated tool is refused
with the `SafetyUnsafe` error and cancelled by the unsafe transition,
an ungated one answers and runs to completion. Operators can move any
tool across the line with `safety.gate` (§ Configuration).

### Built-in Tools

**Hardware**

| Action | Class | Parameters | Returns | Description |
|--------|-------|-----------|---------|-------------|
| `capture` | Ungated | camera_id *or* train_id (exactly one), duration, target (optional slug), frame_type (optional: `Light`/`Dark`/`Flat`/`Bias`) — see [Capture Tool Details](#capture-tool-details) | image_path, document_id | Take an exposure, download `image_array`, save FITS file, create exposure document. `train_id` resolves the train's terminal camera; everything downstream — the `optics` block, gate membership, events — follows the resolved camera. Carries an **advisory predicted deadline** on `exposure_started`: `predicted = duration + camera.readout_time_estimate` (default 15 s when unset), `max = predicted + 30 s` readout headroom. rp does **not** enforce this (the camera driver owns the exposure); it rides the envelope as `predicted_duration_ms`/`max_duration_ms` for the Sentinel watchdog. rp's own readout backstop (a separate, more generous `duration + 120 s` ceiling) is unchanged. Through a camera terminating an imaging train, holds the [mount motion gate](#mount-motion-gate) shared for the whole pipeline (a pending mount motion delays the start) |
| `get_camera_info` | Ungated | camera_id | max_adu, exposure_min, exposure_max, sensor_x, sensor_y, bin_x, bin_y, gain, offset | Read camera capabilities and current settings. `gain` and `offset` are read live from the device; `null` means exactly that the driver does not implement the property (ASCOM `NotImplemented`), and any other read failure is a tool error so a transport blip is never persisted as "no gain" — a flat-timing record is only valid at the gain it was trained at (calibrator-flats-provider plan, D4/D5) |
| `move_focuser` | Ungated | focuser_id, position | actual_position | Move focuser to absolute position (blocks polling `is_moving` until idle). Bounded by a **predicted deadline**: `predicted = \|target − current\| / focuser.steps_per_sec` (current position read before the move); `max = max(predicted × 2, MIN_FOCUSER_DEADLINE = 5 s)`. If the pre-move read fails it falls back to a 120 s ceiling; `predicted`/`max` ride the `move_focuser_started` envelope as `predicted_duration_ms`/`max_duration_ms` |
| `get_focuser_position` | Ungated | focuser_id | position | Read current focuser position |
| `get_focuser_temperature` | Ungated | focuser_id | temperature_c | Read focuser temperature sensor |
| `move_rotator` | Ungated | rotator_id *or* train_id (exactly one), angle | rotator_id, angle, mechanical_angle, moved_trains | Move the rotator to an absolute **sky** angle in degrees (`0.0 ≤ angle < 360.0`, the ASCOM `Position` frame), blocking on `IsMoving` until idle (fixed 120 s ceiling; no predictive deadline — there is no rotator rate config yet). `train_id` resolves the train's sole rotator. `moved_trains` lists every train containing the rotator. See [Rotator Tool Details](#rotator-tool-details) |
| `get_rotator_position` | Ungated | rotator_id *or* train_id (exactly one) | rotator_id, angle, mechanical_angle, is_moving | Read the rotator's sky angle, mechanical angle, and motion state |
| `slew` | Gated | ra, dec, settle_after (optional) | actual_ra, actual_dec | Slew the singular mount to coordinates (blocks until `Slewing == false` plus configured / per-call settle). Tracking must be on; ASCOM error propagates otherwise. Bounded by a **predicted deadline**: `predicted = great-circle(current, target) / mount.slew_rate_arcsec_per_sec + settle`; `max = max(predicted × 3, MIN_SLEW_DEADLINE = 30 s)`. The current pointing is read before the slew to size the deadline; if that read fails it falls back to a 300 s ceiling. On timeout `slew` best-effort aborts (unlike `park`); `predicted`/`max` ride the `slew_started` envelope as `predicted_duration_ms`/`max_duration_ms`. Takes the [mount motion gate](#mount-motion-gate) exclusively — in-flight imaging-train exposures complete first |
| `sync_mount` | Ungated | ra, dec | — | Sync mount position to given coordinates |
| `get_mount_position` | Ungated | — | ra, dec | Read the mount's current pointing |
| `get_tracking` | Ungated | — | tracking, can_set_tracking | Read tracking state and `CanSetTracking` capability; fails loud on read error |
| `set_tracking` | Gated | enabled | — | Enable or disable sidereal tracking |
| `park` | Ungated | — | — | Park the mount (blocks polling `AtPark` every 100 ms until it returns `true`). Bounded by a **predicted deadline**: rp can't read the park position via Alpaca, so it sizes a worst-case full-axis traverse — `predicted = 180° / mount.slew_rate_arcsec_per_sec + settle`; `max = max(predicted × 2, MIN_PARK_DEADLINE = 60 s)` (falls back to a 300 s ceiling with no mount configured); `predicted`/`max` ride the `park_started` envelope. `AtPark` is the ASCOM-canonical completion signal — `Slewing` is sticky on `MoveAxis` rate state and unrelated `SlewState` activity, so polling it would be over-conservative. Per ASCOM, a successful park clears `Tracking`. Unlike `slew`, does NOT auto-abort on timeout — call `abort_slew` to interrupt a stuck park. Exempt from the [mount motion gate](#mount-motion-gate): a terminal/emergency action must never queue behind an exposure |
| `unpark` | Gated | — | — | Clear the mount's `AtPark` flag. Returns immediately. Does NOT auto-enable `Tracking`; call `set_tracking` before slewing |
| `get_park_state` | Ungated | — | at_park, can_park, can_unpark | Read park state and capabilities; fails loud on `AtPark` read error |
| `abort_slew` | Ungated | — | — | Abort an in-progress mount slew or park. Per ASCOM, only valid while `Slewing == true`; the natural Alpaca error propagates otherwise |
| `set_filter` | Ungated | filter_wheel_id *or* train_id (exactly one), filter_name | filter_wheel_id, filter_name, position | Change filter wheel position. `train_id` requires the train to contain exactly one filter wheel — none is an error naming the train, several is ambiguous and also an error (the sole-rotator rule of `move_rotator`, applied to wheels); the result and `filter_switch` event carry the resolved `filter_wheel_id` |
| `get_filter` | Ungated | filter_wheel_id | filter_name, position | Read current filter |
| `get_cover_state` | Ungated | calibrator_id *or* train_id (exactly one) | calibrator_id, trains, cover_state | Read the cover state (`NotPresent` \| `Closed` \| `Moving` \| `Open` \| `Unknown` \| `Error`) without actuating anything — e.g. so an orchestrator can restore the state it found. `train_id` resolves the train's cover calibrator; `trains` lists every train containing it. See [CoverCalibrator Tool Details](#covercalibrator-tool-details) |
| `close_cover` | Ungated | calibrator_id *or* train_id (exactly one) | calibrator_id, trains, status | Close the dust cover (blocks until closed) |
| `open_cover` | Gated | calibrator_id *or* train_id (exactly one) | calibrator_id, trains, status | Open the dust cover (blocks until open) |
| `calibrator_on` | Ungated | calibrator_id *or* train_id (exactly one), brightness (optional) | calibrator_id, trains, status, brightness | Turn on flat panel at brightness (0..max_brightness, default max). Blocks until ready |
| `calibrator_off` | Ungated | calibrator_id *or* train_id (exactly one) | calibrator_id, trains, status | Turn off flat panel. Blocks until off |
| `get_train_info` | Ungated | train_id | train_id, purpose, focal_length_mm, camera_id, filter_wheel_id, filters, calibrator_id, focusers, rotator_id, devices | Describe an optical train without touching any device: the terminal camera, the sole filter wheel with its configured filter names in position order (`filter_wheel_id` and `filters` both `null` when the train has none or several), the cover calibrator (`null` when none), the focusers in optical order, the sole rotator (`null` when none or several), and the ordered `devices` list as `{id, kind}`. An unknown train is an error naming it. See [Optical Trains](#optical-trains) |

**Cooling** (see [Camera Cooling](#camera-cooling))

| Action | Class | Parameters | Returns | Description |
|--------|-------|-----------|---------|-------------|
| `start_cooldown` | Ungated | — | cameras | Run the setpoint-ladder cooldown pass for every camera with a `cooler_targets_c` ladder, in the background; returns the camera ids it is driving (empty when no camera has a ladder). Idempotent: a running pass is left alone, a cooler already regulating at a ladder rung is adopted without re-selection, a running warm-up is cancelled and superseded |
| `start_warmup` | Ungated | — | cameras | Ramp every camera `rp` is cooling warm (+5 °C steps, then cooler off), in the background; returns the camera ids it is warming (empty when `rp` commands none). Idempotent: a running ramp is left alone |

**Guider**

| Action | Class | Parameters | Returns | Description |
|--------|-------|-----------|---------|-------------|
| `start_guiding` | Gated | recalibrate (optional), settle_pixels / settle_time / settle_timeout (optional; per-call > `equipment.mount.guiding` config > service default, field by field) | state, rms_ra_px, rms_dec_px, total_rms_px, sample_count | Start guiding loop, block until settled |
| `stop_guiding` | Ungated | — | state | Stop guiding loop, block until confirmed (idempotent) |
| `dither` | Gated | pixels (optional; falls back to the guiding config's `dither_pixels`), unit (optional: `guide_px` default \| `main_px` \| `arcsec`), ra_only (optional), settle_* as in `start_guiding` | state, rms_ra_px, rms_dec_px, total_rms_px, sample_count | Send dither command, block until re-settled. `unit` interprets the per-call `pixels` amount; rp converts to guide-camera pixels via train pixel scales — see the note below. Takes the [mount motion gate](#mount-motion-gate) exclusively — in-flight imaging-train exposures complete first |
| `pause_guiding` | Ungated | full (optional) | state | Pause guide corrections (e.g., during readout); `full` also pauses looping |
| `resume_guiding` | Gated | — | state | Resume paused guiding |
| `get_guiding_stats` | Ungated | — | app_state, guiding, rms_ra_px, rms_dec_px, total_rms_px, snr, star_mass, sample_count | Read current guiding statistics (cheap; safe to poll) |

The guider *service* always receives **guide-camera pixels** (PHD2's
own pixel scale only exists after calibration, so the service accepts
no other unit), and every guider quantity rp reports back (RMS,
settle thresholds) stays in guide-camera pixels. `dither`'s optional
`unit` is a conversion rp performs before the proxy call, using the
train model's pixel-scale derivation
`scale_arcsec_per_px = 206.265 × pixel_size_x_um / focal_length_mm`
(train `focal_length_mm` + the camera's connect-time pixel-size read;
square pixels assumed — the x-axis size is used):

- `arcsec` divides the amount by the **guiding train's** scale;
- `main_px` first multiplies by the **imaging train's** scale to get
  arcseconds, then divides by the guiding train's. It requires
  exactly one imaging train — with several (piggyback rig), pass
  `arcsec` or `guide_px` instead;
- a non-default `unit` requires an explicit per-call `pixels` amount
  (the `dither_pixels` config default is always guide-camera pixels),
  and the error names whichever conversion input is missing: no
  guiding train, a train without `focal_length_mm`, or a camera whose
  pixel size is unavailable (connect-time read failed or camera never
  connected).

The tools proxy to the guider service and error with
"guider not configured" when the `equipment.mount.guiding` config
block is absent.

**Safety**

| Action | Class | Parameters | Returns | Description |
|--------|-------|-----------|---------|-------------|
| `get_safety_status` | Ungated | — | overall, since, monitors: \[{id, state, since}\], gated | The safety state the gate acts on, for a client that does not consume SSE: `overall` (`safe` \| `unsafe`) with the time it last changed, every configured monitor's last reading (`safe` \| `unsafe` — a failed read counts as unsafe) with the time *it* last changed, and the effective gated tool list after `safety.gate` overrides. See § Safety → [In-Flight Tool Calls](#in-flight-tool-calls); `safety_changed` stays the push signal |

**Compute (image analysis)**

All image analysis tools accept either `document_id` (resolved via the
[Image and Document Cache](#image-and-document-cache), avoiding FITS
decode) or `image_path` (read from disk via `rp-fits`). Where both are
accepted, `document_id` takes precedence.

| Action | Parameters | Returns | Description |
|--------|-----------|---------|-------------|
| `compute_image_stats` | document_id or image_path | median_adu, mean_adu, min_adu, max_adu, pixel_count | Pixel-level statistics. Implemented. |
| `measure_basic` | document_id or image_path, threshold_sigma (optional) | hfr, star_count, background_mean, background_stddev | Detect stars, compute aggregate HFR and background. **MVP image analysis tool.** |
| `detect_stars` | document_id or image_path, min_area, max_area, threshold_sigma (optional) | stars: \[{x, y, flux, peak, saturated_pixel_count}\], star_count, saturated_star_count, background_mean, background_stddev | Locate stars via thresholded connected-components on background-subtracted pixels. Implemented. |
| `measure_stars` | document_id or image_path, min_area, max_area, threshold_sigma (optional), stamp_half_size (optional) | stars: \[{x, y, hfr, fwhm, eccentricity, flux}\], star_count, median_fwhm, median_hfr, background_mean, background_stddev | Per-star photometry and PSF metrics. Runs `detect_stars` internally; the optional `stars` input from the catalog row is deferred. Implemented. |
| `estimate_background` | document_id or image_path, k (optional), max_iters (optional) | mean, stddev, median, pixel_count (sigma-clipped) | Robust background estimation. Implemented. |
| `compute_snr` | document_id or image_path, min_area, max_area, threshold_sigma (optional) | snr, signal, noise, star_count, background_mean, background_stddev | Median per-star SNR via the CCD-equation approximation. Implemented. |

**Compute (plate solving)**

| Action | Parameters | Returns | Description |
|--------|-----------|---------|-------------|
| `plate_solve` | document_id or image_path; optional pointing_hint, use_mount_hints, fov_hint_deg, search_radius_deg, timeout | ra_center, dec_center, pixel_scale_arcsec, rotation_deg, solver, wcs_matrix | Solve an image. Proxies to the plate-solver rp-managed service (which wraps ASTAP). Persists a `wcs` section to the exposure document. See [`plate_solve` Contract](#plate_solve-contract). |

**Compound (built-in)**

Compound tools drive a multi-step workflow internally using the primitive
built-in tools. They live in `rp`'s process — no MCP hop, no plugin
boundary — but expose the same MCP tool surface as any other tool.

| Action | Class | Parameters | Returns | Description |
|--------|-------|-----------|---------|-------------|
| `auto_focus` | Ungated | camera_id + focuser_id *or* train_id (mutually exclusive); duration, step_size, half_width, min_area, max_area, threshold_sigma (optional), min_fit_points (optional) — with train_id, per-call sweep parameters fall back field by field to the train's `auto_focus` config block | best_position, best_hfr (capture sweep) / best_hfd (metric sweep), final_position, samples_used, curve_points, temperature_c | Parabolic-fit V-curve auto-focus. Imaging addressing drives `move_focuser` + `capture` + `measure_basic` internally; addressing the **guiding train** runs the PHD2-metric sweep instead (median HFD of fresh guide frames per position; requires active guiding; never captures through the guide camera). See [`auto_focus` Contract](#auto_focus-contract). Implemented. |
| `refocus_train` | Ungated | train_id, reason (optional) | train_id, reason, guiding_paused, steps | Expand one refocus trigger into the train model's dependency-ordered AF sequence — shared focusers upstream-first (each run in the train where it is terminal), then the train's own terminal focuser — pausing guide corrections around the sequence when a step moves a guiding-train focuser. Sweep parameters come from each run train's `auto_focus` config block. See [`refocus_train` Contract](#refocus_train-contract). |
| `center_on_target` | Gated | camera_id *or* train_id (exactly one), ra, dec, duration, tolerance_arcsec, max_attempts | final_error_arcsec, attempts, final_ra, final_dec, iterations | Iterative `capture` + `plate_solve` + `sync_mount` + `slew` loop until residual ≤ `tolerance_arcsec`. `train_id` resolves the train's terminal camera. Carries an **advisory outer-loop deadline** on `centering_started`: `per_iter = duration + centering.solve_time_estimate + centering.slew_overhead_estimate`, `predicted = per_iter`, `max = max_attempts × per_iter`. The watchdog tracks only this outer loop; each inner `slew`/`capture` carries its own deadline, and each takes the [mount motion gate](#mount-motion-gate) in its own mode (slews exclusive, imaging-train captures shared). See [`center_on_target` Contract](#center_on_target-contract). Implemented. |

**Planner — Ephemeris primitives**

One operation each, backed by the `Ephemeris` trait in
`rp-ephemeris`. Times are humantime ISO-8601 strings (e.g.
`"2026-05-03T22:00:00Z"`); when omitted, defaults to "now". `ra` is
hours, `dec` is degrees. See
[Planning and Ephemeris](#planning-and-ephemeris).

| Action | Parameters | Returns | Description |
|--------|-----------|---------|-------------|
| `resolve_target` | name | ra_hours, dec_degrees, object_type, magnitude, size_arcmin | Catalog lookup against the embedded deep-sky + star catalog (see [Catalog](#catalog-rp-catalog)) |
| `compute_alt_az` | ra, dec, time (optional) | altitude_degrees, azimuth_degrees | Topocentric alt/az for an ICRS target |
| `compute_transit` | ra, dec, date (UTC `YYYY-MM-DD`) | transit_utc | UT of upper transit on a given UTC date |
| `compute_rise_set` | ra, dec, date (UTC), min_alt_degrees | rise_utc, set_utc | Rise/set times above a given altitude (null for circumpolar / never-up) |
| `compute_meridian_flip` | ra, dec, time, side_of_pier | time_to_flip_seconds | Time-to-flip from current side of pier (seconds) |
| `get_sun_position` | time (optional) | ra_hours, dec_degrees, altitude_degrees, azimuth_degrees | Sun position |
| `get_twilight` | date (UTC), kind | kind, begin_utc, end_utc | Civil / nautical / astronomical twilight window |
| `get_moon_position` | time (optional) | ra_hours, dec_degrees, altitude_degrees, azimuth_degrees, phase_degrees, illumination_fraction | Moon position + Sun-Moon elongation (phase) |
| `compute_moon_separation` | ra, dec, time (optional) | separation_degrees | Angular separation between target and Moon |
| `get_local_sidereal_time` | time (optional) | lst_hours | Local sidereal time at the configured site |
| `get_site` | — | latitude_degrees, longitude_degrees | The configured observer site (cross-checked against the mount on connect when the mount reports one); lets a plugin source coordinates instead of duplicating them |

**Planner — Convenience tools**

| Action | Parameters | Returns | Description |
|--------|-----------|---------|-------------|
| `get_next_target` | time (optional), train_id (optional) | target, reason, exposure | Evaluate candidates and recommend next target. `target` nests its coordinate as `coord: {ra_hours, dec_degrees}`; `exposure` is a nested `{filter, duration_secs}` object from the recommended target's first **incomplete** goal (the derived on-disk progress rotates the plan), or null when the target defines none. Reads the imaging train's filter wheel (`train_id`) — or the rig's only wheel — for the filter-batching tie-break — see §"Dynamic Planner" |
| `get_target_status` | target_name *or* (ra + dec); time (optional) | target_name, altitude_degrees, azimuth_degrees, hour_angle_hours, time_to_set_seconds, progress | Sky position + progress for a catalog target or raw ICRS coords. `progress` is the per-goal `{filter, binning, exposure_duration, desired_count, good, total}` list when `target_name` (as given or catalog-resolved) matches an active target-store row, null otherwise (including the ra/dec form) — see [Target Store § Progress derivation](#progress-derivation) |
| `get_meridian_status` | time (optional) | time_to_flip_seconds, side_of_pier, mount_ra_hours, mount_dec_degrees | Time-to-flip + side-of-pier from the mount's current pointing |
| `record_exposure` | target, filter (optional) | target, filter, progress | Read the target's derived progress back after a frame. It increments nothing — `capture` already wrote the frame — and records nothing: the filter-batching tie-break reads the wheel. `target` must name an active target-store row (its slug); `filter` is echoed back (omit it, or pass null / `""`, for an unfiltered frame) — see [Target Store § Progress derivation](#progress-derivation) |
| `get_session_progress` | — | progress | Full progress overview: target slug → the per-goal `{filter, binning, exposure_duration, desired_count, good, total}` list, for every active target-store row |

**Targets**

`add_target`, `get_target`, `list_targets`, `update_target`,
`delete_target`, `set_goals` — CRUD over the plan-data store where
targets now live. See [Target Store](#target-store) for the full
contract.

**Session**

There are no session-state tools: `rp` keeps no run state (progress
needs no persisting at all — it is derived from the frames on disk, see
[What Survives an rp Restart](#what-survives-an-rp-restart)), and an
orchestrator's own resume state is its own concern (`session-runner`'s
blackboard). An earlier design sketched `save_session_state` /
`get_session_state` tools; they were dropped — no orchestrator had a
use for them that the planner's `get_session_progress` doesn't already
cover.

All built-in tools validate parameters before execution. `move_focuser`
checks position bounds. `capture` checks that the camera is connected and
idle. Invalid requests return an MCP error — they never reach the
hardware.

#### Capture Tool Details

The `capture` tool takes exposure time as a humantime string (`duration`,
e.g. `"500ms"`, `"30s"`, `"1m30s"`).
After the exposure completes and `image_ready` returns true, `capture`
downloads the camera's `image_array`, writes it as a FITS file via
`rp-fits` (BITPIX=16+BZERO=32768 for the common 16-bit sensor case;
BITPIX=32 when `max_adu > u16::MAX`) with `DOC_ID = '<full-uuid>'` in the
primary HDU header, and creates a sidecar exposure document JSON
alongside it. The base filename is `<doc_uuid_8>`; both files share
that base (`<doc_uuid_8>.fits` and `<doc_uuid_8>.json`). Both are
written atomically (stage to a sibling temp file, fsync, rename, fsync
parent directory). See
[Persistence](#persistence) for the full rule set.

**Target linkage (Decision 11 — landed).** `capture` gains two optional
parameters: `target` (a slug string) and `frame_type`
(`Light`/`Dark`/`Flat`/`Bias`). `rp` itself has no session-side notion
of "the current target" — that state lives entirely in the
orchestrator's workflow (`session-runner`'s blackboard:
`session.target_name`/`session.target_ra`/`session.target_dec` in
`deep_sky.json`), which already sets it right after every `slew` and
re-supplies it explicitly to whichever tool call needs it next (today,
that's only `record_exposure`, called immediately after `capture` in
the same workflow step). Adding `target` to `capture`'s own schema is
the same idiom applied one tool call earlier — no new subsystem, no
rp-side session-target tracking — the workflow already holds the value
at the moment `capture` runs; it was simply never passed.

`frame_type` switches the document stamping: omitted (the default),
`capture` behaves exactly as before — a flat `<doc_uuid_8>.fits`, no
`target`/`frame_type` on the exposure document, `target` ignored if
somehow supplied anyway. This is what every caller that hasn't been
updated for Decision 11 keeps doing unchanged, including `auto_focus`'s
and `center_on_target`'s internal captures (see below) and any
orchestrator predating this feature.

**The templated path is a second, independent switch:
`session.file_naming_pattern`** (§ Persistence). Supplying `frame_type`
without it is *not* an error — the document still records what the
frame is and what it is of, and the file keeps the flat
`<doc_uuid_8>.fits` name. The two are deliberately decoupled because
the shipped `deep_sky` workflow passes `frame_type: Light` on every
frame: coupling them would make a rig that never configured a naming
pattern fail every capture the moment it adopted the current workflow.
The cost of leaving the pattern unset is that nothing on disk carries a
target, so no progress can be derived from it and the planner never
exhausts a goal (§ Progress derivation) — a rig running plans wants the
pattern configured.

When `frame_type: Light`, `target` is **required** — a Light frame with
no target has no sensible directory bucket, and only Light frames
bucket against `AcquisitionGoal` quotas (rp-targets.md § File-naming
template). The slug is resolved against the target store (an unknown
slug or an absent store both error); `capture` denormalizes
`slug`/`display_name`/`ra_hours`/`dec_degrees` onto the document's
`target` field (§ Exposure Document).

When `frame_type` is `Dark`/`Flat`/`Bias` (calibration frames —
`calibrator-flats`' `take_flats` and any future dark/bias capture
flow, neither of which images a sky object), `target` is
optional: if supplied it resolves against the store exactly like a
Light frame (reserved for a future per-target flat-capture flow, see
below); if omitted, `capture` uses a **reserved slug equal to the
lowercased frame type** (`"dark"`/`"flat"`/`"bias"`) — a single shared
bucket per calibration type, `target` on the document carrying just
that slug (`display_name`/`ra_hours`/`dec_degrees` stay `None`, since
it names no real target-store row).

*Filter resolution.* `{filter}`/`{filter_position}` need a live read
from the resolved camera's train filter wheel, but a train may have no
filter wheel at all (mono/OSC rigs), and dark current isn't
filter-dependent regardless of what happens to be selected. Rule: for
`Light` and `Flat`, `capture` reads the train's current filter
name/position live when a filter wheel is present, else renders the
fixed literal `"NA"` / position `0`. For `Dark`/`Bias`, `capture`
always renders `"NA"`/`0`, even when a wheel is present — recording an
incidental filter position on a dark/bias would be noise, not signal.

*Directory/file rendering.* Once `target`/`frame_type` are resolved,
`capture` renders `session.directory_pattern` then
`session.file_naming_pattern` (§ Persistence) to produce the final
on-disk path, replacing the flat `<doc_uuid_8>.fits`. `{night_date}`
uses the noon-rollover rule against the capture-completion instant
(rp-targets.md § Progress derivation); `{frame_number}` is derived by
scanning the target frame's directory for existing files sharing the
same `(filter, binning, exposure_duration)` sub-spec via
`CompiledTemplate::parse` and using `count + 1` — nothing is stored,
consistent with the "derive progress from disk" design (rp-targets.md
§ Progress derivation). A render failure (a missing field, a filter
name outside its token shape, a failed sensor-temperature read when
the pattern references `{sensor_temp}`) fails the whole `capture`
call — after the exposure has already completed, since
`{sensor_temp}` is measured at capture completion. This trades a
wasted exposure on a misconfiguration for never silently mis-filing a
frame; the operator fixes the configuration and retries.

**Deferred, not yet decided:** organizing `auto_focus`'s and
`center_on_target`'s internal diagnostic captures the same way. Unlike
calibration frames, these can run multiple times against the same
target in one night (repeated focus/centering attempts), so they'd
need a directory shape that doesn't exist yet (something like
`_diagnostics/<train>/auto_focus/...`) and a naming-template token at
finer-than-`{night_date}` granularity — there is no `{time}` token in
[`rp-targets.md` § File-naming template](../crates/rp-targets.md#file-naming-template-render-and-parse)
today. Both tools keep calling `capture` with `frame_type` omitted
(today's flat-file behavior) until this is designed.

**Sidecar failure contract.** If the sidecar write fails after a
successful FITS write, `capture` still returns success with
`image_path` and `document_id` — the FITS file remains on disk and is
the durable record. The cache insert is gated on sidecar success, so
no in-memory entry is created; the disk-fallback resolver also cannot
rehydrate (it requires the sidecar to recover `max_adu` and other
document fields), so subsequent `document_id`-keyed lookups
(`/api/documents/{id}`, `/api/images/{id}`, `/pixels`, image-analysis
tools called with `document_id`) return 404. `rp` emits a
`document_persistence_failed` event carrying `document_id`,
`file_path`, and the error. Subscribers and operators recover by
reading the FITS directly via `file_path` (e.g. image-analysis tools
called with `image_path` instead of `document_id`).

#### CoverCalibrator Tool Details

The CoverCalibrator tools control flat panel devices. `calibrator_on`
accepts an optional `brightness` parameter (0 to `max_brightness`). When
omitted, the calibrator is turned on at maximum brightness. The four
actuating tools block until the operation completes by polling the
device state (same pattern as `set_filter`), bounded by a 60 s ceiling.

**Addressing.** All five tools take **exactly one of `calibrator_id`
or `train_id`** — the `set_filter` shape (each tool's input schema
publishes the alternatives as a presence-only `oneOf`). `train_id`
resolves the cover calibrator that is first in the train's device list
([Optical Trains](#optical-trains)); a train without one is an error
naming the train (`train 'guide' has no cover calibrator`), an unknown
train is `train not found`, and naming both or neither is an error
naming the tool. Device-id addressing stays first-class: a calibrator
outside every train is still driven by its `calibrator_id`.

**`trains` on every result.** Each result carries the resolved
`calibrator_id` and `trains`, the ids of every optical train containing
the calibrator, in config order — empty for a calibrator in no train.
A closed cover blinds every camera behind it and a lit panel floods
them, so a caller closing the main train's flip-flat learns that the
OAG guide train went dark too (the `moved_trains` precedent of
`move_rotator`).

**Gating is unchanged by addressing.** `open_cover` is gated (it
exposes the optics); the other four are ungated — reads, protective
or indoor — whichever way they are addressed.

#### Rotator Tool Details

`move_rotator` moves the rotator to an absolute **sky** angle — the
ASCOM `Position` frame, which honors any sync offset the driver
holds — in degrees, `0.0 ≤ angle < 360.0`. The angle is validated
before any motion; a non-finite or out-of-range value errors without
touching the device. The tool then polls `IsMoving` every 100 ms
until idle, bounded by a fixed 120 s ceiling: there is no
per-rotator rate config yet, so the `move_rotator_started` envelope
carries no `predicted_duration_ms`/`max_duration_ms` (the standard
posture for operations not yet converted to predictive deadlines).
On idle it reads back `Position` and `MechanicalPosition` for the
result.

`moved_trains` on the result lists every optical train containing
the rotator — the trains whose field orientation the move changed.
When the list includes the guiding train, the guider is configured,
and the guider's stats report an active guide loop, `move_rotator`
runs the **rotate-while-guiding ladder** around the move:

1. **Pause guide corrections** (output-only — looping continues, so
   the re-selection below has fresh frames). A pause failure aborts
   the tool before any motion. The pause and the pre-move read are
   part of the operation: `move_rotator_started` is emitted before
   them, and their failures emit `move_rotator_failed` like any
   other leg.
2. Read the pre-move sky angle, then run the move as usual.
3. **Calibration decision**: query the guider service for PHD2's
   equipment. When PHD2 reports a **connected rotator**, stop there —
   PHD2 stores the rotator angle with each calibration and adjusts
   for the current angle when guiding resumes, exact for any Δθ.
   Otherwise, when |Δθ| (the sky-angle change, folded to
   [0°, 180°]) exceeds
   `equipment.mount.guiding.recalibrate_above_deg` (default 5°),
   clear PHD2's mount calibration — PHD2 recalibrates on the next
   guide start. At or below the threshold the calibration is kept:
   the cross-axis leak (sin Δθ) sits inside guiding's noise floor.
4. **Re-select the guide star** (the rotation moved it), then
   **resume corrections**.

A move failure still runs re-select + resume (the field may have
partially rotated) and reports the move error; a resume failure is a
hard tool error on top of whatever preceded it — mirroring
`refocus_train`'s handshake contract. When guiding is not active,
the stats read fails, or no guider is configured, the move runs bare
(Tenet 2: a mid-day rotation must not fail because PHD2 is closed).
The result gains `guiding_ladder`: `null` when the ladder did not
engage, else `{ "phd2_has_rotator": bool, "delta_deg": number,
"calibration_cleared": bool }`.

Both rotator tools address the device as `rotator_id` *or*
`train_id` — exactly one; passing both or neither is an error.
`train_id` resolves through the train model and requires the train
to contain exactly one rotator: none is an error naming the train,
and several (physically exotic, but not rejected by validation) ask
the caller for the explicit `rotator_id`.

#### Image Statistics Tool Details

`compute_image_stats` computes median, mean, min, and max ADU values
on the captured image. It accepts either a `document_id` (resolved
through the unified image+document cache, falling back to disk scan
on miss) or an `image_path` (read from disk via `rp-fits`); when both
are supplied, `document_id` wins. When called with a `document_id`,
the stats are written into the exposure document as an `image_stats`
section. This tool does not access the camera — it operates on saved
image files.

### Image Analysis Strategy

Image analysis in `rp` follows a **pure Rust on ndarray** approach.
All algorithms are implemented as custom code on top of well-established
building blocks — no single crate covers the full range of astronomical
image analysis needed. Tools accept either a `document_id` (resolved
via the [Image and Document Cache](#image-and-document-cache)) or an
`image_path` (FITS file on disk read via `rp-fits`); `document_id` is
preferred for the post-capture fast path because it avoids re-decoding
the image just written.

#### Current Capabilities

- **Pixel statistics** (median, mean, min, max ADU) — stdlib iterators
  and `select_nth_unstable` for median (iterative O(n) quickselect).
  Used by `compute_image_stats` for flat calibration exposure targeting.
- **FITS I/O** — `rp-fits` workspace crate (reads via `fitsrs`, writes
  via a hand-rolled pure-Rust BITPIX 8/16/32 writer). See ADR-001
  Amendment A.

#### Planned Capabilities and Crate Strategy

| Capability | Approach | Crates |
|------------|----------|--------|
| Pixel statistics | Custom | stdlib (`select_nth_unstable`, iterators) |
| FITS I/O | Crate | `rp-fits` (wraps `fitsrs` for reads, hand-rolled writer) |
| 2D image operations | Crate | `ndarray` (already in workspace) |
| Gaussian smoothing, morphology | Crate | `ndarray-ndimage` (Gaussian filter, dilation/erosion). Connected components is hand-rolled BFS on `Array2<bool>` because `ndarray-ndimage` 0.6's `label` is 3D-only |
| Star detection | Custom | Threshold + connected components on background-subtracted image, then shape filtering |
| Centroiding | Custom | Intensity-weighted center of mass on ndarray subframes |
| HFR / HFD | Custom | Radial flux accumulation (~20 lines of math) |
| FWHM | Custom + crate | 2D Gaussian fitting via `rmpfit` (chosen over `levenberg-marquardt` for native parameter bounds — σ > 0, amplitude > 0 — and lighter dependency footprint: no `nalgebra`. `rmpfit` is also a Rust port of MPFIT, the de-facto astronomy fitting library) |
| Eccentricity / elongation | Custom | Second central moments from detected star pixels |
| Background estimation | Custom | Sigma-clipped mesh statistics on ndarray |
| Noise / SNR | Custom | Sigma-clipped statistics |

#### MVP: `measure_basic` Contract

The first analysis tool to implement. Behavioral contract:

**Input**:
- `document_id` (preferred — resolves to cached pixels) **or** `image_path`
  (FITS file on disk).
- `min_area` — minimum component pixel area to admit as a star. Required;
  no default. The right value depends on the camera+optics pixel scale
  (arcsec/px) and the seeing regime, neither of which the tool can infer
  from the image alone. Callers (workflows, plugins) own that policy.
- `max_area` — maximum component pixel area to admit. Required; no
  default. Same rationale as `min_area`. Note: at extreme defocus,
  donut-shaped PSFs from the secondary obstruction can span many hundreds
  of pixels — auto-focus callers should set `max_area` accordingly so
  the V-curve sweep can measure them.
- Optional `threshold_sigma` (default `5.0`) — detection threshold above
  background. Unit-free (multiples of the sigma-clipped background
  stddev), so a default is meaningful here.

**Output**:
- `hfr` — half-flux radius in pixels, aggregated across detected stars
  (median of per-star HFRs). `null` if no stars detected.
- `star_count` — number of valid stars after detection and filtering.
- `saturated_star_count` — number of detected stars that contain at
  least one pixel at `max_adu`. `0` when `max_adu` is unknown (e.g. when
  called via `image_path` outside an exposure context).
- `background_mean` — sigma-clipped background mean (ADU).
- `background_stddev` — sigma-clipped background standard deviation (ADU).
- `pixel_count` — total pixels analyzed.

**Algorithm (in order)**:
1. Load pixels (image-cache hit or `rp-fits` read).
2. Estimate background via sigma-clipped mean/stddev.
3. Apply Gaussian smoothing (small kernel, σ ≈ 1.0 px) to suppress noise.
4. Threshold at `background_mean + threshold_sigma × background_stddev`.
5. Connected-components labelling on the thresholded mask.
6. Filter components: pixel area in `[min_area, max_area]`; reject
   components touching the image border. Saturated components are *not*
   rejected — they are flagged (see `saturated_star_count`). Saturated
   stars carry real signal: bright in-focus stars routinely clip at
   long-enough exposures, and donut-shaped PSFs at extreme defocus are
   usually saturated in their bright annulus. Filtering them out would
   make HFR-vs-focus non-monotonic and break auto-focus, so the policy
   is to measure them and let downstream consumers decide whether to
   weight or warn.
7. For each surviving component, compute intensity-weighted centroid and
   per-star HFR (radial flux accumulation to half of total flux).
   Centroiding uses background-subtracted flux to avoid bbox-center bias.
8. Return aggregate HFR (median of per-star HFRs), star count,
   saturated-star count, and background.

**Error cases**:
- Neither `document_id` nor `image_path` provided → MCP error mentioning
  `image_path` (the most fundamental missing input).
- `image_path` provided but file not found → MCP error.
- `document_id` provided but neither cache nor FITS-on-disk fallback
  resolves → MCP error.
- `min_area` or `max_area` missing → MCP error naming the missing field.
  These parameters are deserialized as optional and validated by the tool
  body in this order — `document_id`/`image_path` first, then `min_area`,
  then `max_area` — so the error message tracks the first thing the user
  needs to fix.
- Background estimation fails (e.g. all pixels saturated) → MCP error.
- No stars detected → return successfully with `hfr: null`,
  `star_count: 0`, `saturated_star_count: 0`, populated background fields.
  Not an error — the caller decides whether that's a failure (focus run)
  or fine (cloudy frame still useful for stats).

**Persistence**: when called with `document_id`, results are written into
the exposure document as the `image_analysis` section per the rule that
"all tool results that produce image metrics MUST be written into the
exposure document as a section."

#### `estimate_background` Contract

A focused tool that returns sigma-clipped background statistics on their
own — useful for flat-field analysis, sky-quality screening, and any
caller that wants the background number without paying for star detection.

**Input**:
- `document_id` (preferred — resolves to cached pixels) **or** `image_path`
  (FITS file on disk).
- Optional `k` (default `3.0`) — sigma-clip threshold in stddev units.
- Optional `max_iters` (default `5`) — maximum clip iterations.

**Output**:
- `mean` — sigma-clipped background mean (ADU).
- `stddev` — sigma-clipped background standard deviation (ADU).
- `median` — median of the surviving (post-clip) pixel set (ADU).
- `pixel_count` — total pixels analyzed (input area, not the surviving set).

**Algorithm**: same iterative sigma-clip kernel `measure_basic` uses
internally — clip pixels outside `mean ± k × stddev`, recompute, repeat
until the surviving set stops shrinking or `max_iters` runs out. Median
is taken over the surviving set via `select_nth_unstable`.

**Error cases**:
- Neither `document_id` nor `image_path` provided → MCP error mentioning
  `image_path` (consistent with `measure_basic`).
- `image_path` provided but file not found → MCP error.
- `document_id` provided but neither cache nor FITS fallback resolves →
  MCP error.
- `k <= 0` or `max_iters == 0` → MCP error naming the bad parameter.
- Background estimation fails (e.g. all pixels clipped, empty image) →
  MCP error.

**Persistence**: when called with `document_id`, results are written into
the exposure document as the `background` section. Separate from
`measure_basic`'s `image_analysis` section so the two tools don't
overwrite each other on the same document.

#### `detect_stars` Contract

Returns the per-star list `measure_basic` produces internally — useful for
callers that want star coordinates and fluxes without HFR (centering,
quality screens, custom plate-solver hints). Also persists the list so
follow-up tools (`measure_stars`) can skip re-detection on the same
exposure.

**Input**:
- `document_id` (preferred — resolves to cached pixels) **or** `image_path`
  (FITS file on disk).
- `min_area` and `max_area` — required. Pixel area encodes a pixel-scale
  (arcsec/px) assumption that the tool cannot infer; same rationale as
  `measure_basic` (no defaults).
- Optional `threshold_sigma` (default `5.0`) — detection threshold above
  background, in stddev units.

**Output**:
- `stars` — array of `{x, y, flux, peak, saturated_pixel_count}` objects:
  - `x` / `y` — flux-weighted centroid (pixel coordinates).
  - `flux` — sum of background-subtracted, non-negative flux over the
    component (ADU).
  - `peak` — maximum *raw* pixel value over the component (ADU, not
    background-subtracted). Useful for saturation awareness.
  - `saturated_pixel_count` — pixels at or above the camera's `max_adu`.
    Always `0` when `max_adu` is unknown (bare `image_path` mode).
- `star_count` — convenience aggregate (`stars.length`).
- `saturated_star_count` — count of stars with `saturated_pixel_count > 0`.
- `background_mean` / `background_stddev` — sigma-clipped background used
  to set the detection threshold; included so callers know what cut was
  effectively applied.

**Algorithm**: same pipeline `measure_basic` runs internally — sigma-
clipped background → Gaussian smoothing (σ ≈ 1 px) → threshold at
`mean + threshold_sigma × stddev` → 4-connectivity BFS → area / border
filter → intensity-weighted centroiding. Saturated components are
flagged, not rejected (same rationale as `measure_basic`).

**Error cases**:
- Neither `document_id` nor `image_path` → MCP error mentioning
  `image_path`.
- `min_area` or `max_area` missing → MCP error naming the missing
  parameter (validated in body for deterministic error ordering, same as
  `measure_basic`).
- `image_path` provided but file not found → MCP error.
- `document_id` provided but neither cache nor FITS fallback resolves →
  MCP error.
- Background estimation fails (e.g. empty image) → MCP error.

**Persistence**: when called with `document_id`, the JSON payload is
written to the `detected_stars` section. Separate from `image_analysis`
(measure_basic) and `background` (estimate_background) so all three tools
can run on the same exposure without overwriting each other.

#### `measure_stars` Contract

Per-star photometry and PSF metrics for callers that need FWHM and
eccentricity (auto-focus, guider error budgeting, image-quality screens)
in addition to the HFR / flux that `measure_basic` aggregates.

**Input**:
- `document_id` (preferred — resolves to cached pixels) **or** `image_path`
  (FITS file on disk).
- `min_area` and `max_area` — required (encode pixel-scale assumptions;
  same rationale as `measure_basic` and `detect_stars`).
- Optional `threshold_sigma` (default `5.0`) — detection threshold.
- Optional `stamp_half_size` (default `8`) — half-side of the postage
  stamp used for the 2D Gaussian fit. The fit is rejected for any star
  whose stamp would cross the image boundary.

**Output**:
- `stars` — array of `{x, y, hfr, fwhm, eccentricity, flux}` objects:
  - `x` / `y` — flux-weighted centroid (pixel coordinates).
  - `hfr` — empirical half-flux radius (pixels), or `null` when no
    positive flux above background (rare; `detect_stars` already filters
    this out).
  - `fwhm` — geometric-mean FWHM = 2.3548·√(σx·σy) from the Gaussian
    fit (pixels), or `null` when the fit fails.
  - `eccentricity` — √(1 − (σmin/σmax)²) from the Gaussian fit, or
    `null` when the fit fails.
  - `flux` — sum of background-subtracted, non-negative flux (ADU).
- `star_count` — total stars detected (including those whose fit failed).
- `median_fwhm` — median across stars whose fit succeeded; `null` when
  no fits converged.
- `median_hfr` — median empirical HFR; `null` when no stars detected.
- `background_mean` / `background_stddev` — sigma-clipped background.

**Algorithm**:
1. Sigma-clipped background → `detect_stars` (same pipeline as
   `measure_basic` and `detect_stars`).
2. For each detected star:
   - Empirical HFR over the connected-component pixels (same kernel
     `measure_basic` aggregates).
   - 2D Gaussian fit on a `(2·stamp_half_size+1)²` postage stamp using
     `rmpfit` (Levenberg-Marquardt). Model:
     `I(x, y) = A · exp(−((x−x0)²/(2σx²) + (y−y0)²/(2σy²))) + B`.
     6 free parameters; no rotation (rationale: amateur PSFs rarely
     resolve a meaningful θ at typical pixel scales — geometric-mean
     FWHM and eccentricity capture quality without it).
3. Stars with failed fits keep their row with `fwhm`/`eccentricity` set
   to `null`. They are *not* dropped — the caller decides whether the
   frame is usable.

**Error cases**:
- Neither `document_id` nor `image_path` → MCP error mentioning
  `image_path`.
- `min_area` or `max_area` missing → MCP error naming the missing
  parameter.
- `image_path` provided but file not found → MCP error.
- `document_id` provided but neither cache nor FITS fallback resolves →
  MCP error.
- Background estimation fails (e.g. empty image) → MCP error.

**Persistence**: when called with `document_id`, the JSON payload is
written to the `measured_stars` section. Distinct from `detected_stars`,
`image_analysis`, and `background` so all four tools coexist on one
document.

**Deferred**: the optional `stars` input listed in the tool catalog row
is not implemented in this MVP. When implemented it will let the caller
pass back the array from a previous `detect_stars` call to skip
re-detection; for now, every invocation re-runs detection.

#### `compute_snr` Contract

A signal-to-noise summary across detected stars — the headline number
that quality-screening workflows use to decide whether to keep a frame.

**Input**:
- `document_id` (preferred — resolves to cached pixels) **or** `image_path`
  (FITS file on disk).
- `min_area` and `max_area` — required (encode pixel-scale assumptions;
  same rationale as `measure_basic`, `detect_stars`, and `measure_stars`).
- Optional `threshold_sigma` (default `5.0`) — detection threshold.

**Output**:
- `snr` — median per-star signal-to-noise ratio. `null` when no stars
  are detected.
- `signal` — median per-star background-subtracted total flux (ADU).
  `null` when no stars are detected.
- `noise` — median per-star noise (ADU). `null` when no stars are
  detected.
- `star_count` — number of stars contributing to the medians.
- `background_mean` / `background_stddev` — sigma-clipped background
  used in the noise model.

**Algorithm**: sigma-clipped background → `detect_stars` → for each
star, `signal = total_flux`, `noise = √(signal + N_pix · σ_bg²)`,
`snr = signal / noise`. The aggregate uses the median for robustness
against outliers (saturated stars, hot-pixel spikes).

**Caveats** (kept honest because SNR numbers are easy to misread):
- The noise model collapses dark current and read-noise into the
  background variance and assumes gain ≈ 1 ADU/electron. SNR values are
  comparable across frames from the *same camera*, **not** absolute
  photometric SNRs. Cross-camera comparisons need per-camera gain and
  read-noise inputs that this MVP does not surface.
- Saturated stars are *included* in the median, the same way
  `measure_basic` includes them. Their effective signal is clipped, so
  they bias the median low; aggressive callers can pre-filter via
  `detect_stars` and call `compute_snr` on a subset (deferred — the
  optional `stars` input from `measure_stars` will land here too).

**Error cases**:
- Neither `document_id` nor `image_path` → MCP error mentioning
  `image_path`.
- `min_area` or `max_area` missing → MCP error naming the missing
  parameter.
- `image_path` provided but file not found → MCP error.
- `document_id` provided but neither cache nor FITS fallback resolves →
  MCP error.
- Background estimation fails (e.g. empty image) → MCP error.

**Persistence**: when called with `document_id`, the JSON payload is
written to the `snr` section. Distinct from `detected_stars`,
`measured_stars`, `image_analysis`, and `background` so all five
imaging tools coexist on one document.

#### Design Rationale

This approach follows what N.I.N.A. does: custom astronomical algorithms
on top of general-purpose image processing primitives. The algorithms
(HFR, centroiding, eccentricity) are well-documented and not complex.
SEP (Source Extractor as a library) was considered via `sep-sys` but
rejected due to LGPL license constraints and C FFI maintenance burden.

### Image and Document Cache

The cache is a **first-class API** holding both pixel data and the
exposure document for each capture, evicted as a unit. It serves
built-in tools (in-process, zero-copy), rp-managed services, and
third-party plugins (over HTTP), and eliminates redundant FITS or
sidecar reads for the common post-capture flow where a tool wants to
analyze the image just captured.

When `capture` completes, the camera's pixel array is already decoded
in memory and the document has just been constructed. The cache holds
both so subsequent tools (`measure_basic`, the next iteration of
`auto_focus`, an external analyzer plugin) and document-API consumers
don't re-read from disk. The on-disk FITS+sidecar pair remains the
durable source of truth — the cache is strictly a hot-path
optimization, with the disk as fallback on miss.

Pixels and document share one cache entry. They evict together. Tool
calls that mutate the document (e.g. `measure_basic` writing the
`image_analysis` section) update the cached document under a per-entry
lock and persist the sidecar atomically. After eviction, both the
pixels and the document are gone from memory; either can be rehydrated
from disk on the next access — see
[Document Resolution](#document-resolution).

#### Internal API (built-in tools)

```rust
pub enum CachedPixels {
    U16(Array2<u16>),
    I32(Array2<i32>),
}

pub struct CachedImage {
    pub pixels: CachedPixels,
    pub width: u32,
    pub height: u32,
    pub fits_path: PathBuf,
    pub max_adu: u32,
    pub document: RwLock<ExposureDocument>,
}

ImageCache::insert(document_id: &str, image: CachedImage);
ImageCache::get(document_id: &str) -> Option<Arc<CachedImage>>;
ImageCache::put_section(document_id: &str, name: &str, value: Value)
    -> Result<()>;  // mutates the cached document AND persists sidecar
```

Built-in tools and HTTP handlers that accept a `document_id` resolve
through the cache. On miss, the cache attempts to rehydrate from disk
before returning `None`. Cache misses are logged at `debug!` for
tuning visibility.

#### Document Resolution

A `document_id` (the full UUID) resolves through this order:

1. **Cache hit.** Return the cached entry. O(1).
2. **Disk fallback.** First parse the id: only a UUID can name a
   document (every id `capture` mints is one), so an id that does not
   parse is a miss before any directory is read — the HTTP route
   accepts any path segment, and a typo or a hostile id must not buy a
   tree walk. The UUID-8 to look for is derived from the parsed value
   (`time_low` as eight hex digits, the same derivation `capture` uses
   to mint the suffix). Then walk `<data_directory>` down to
   `session.directory_pattern`'s component depth (0 when no
   `file_naming_pattern` is configured, so only the flat directory is
   read; 3 for the documented default), and at *every* level on the
   way collect filenames matching `<uuid[..8]>.fits` or
   `*_<uuid[..8]>.fits` — the suffix every capture writes (§
   Persistence). Every level, not just the last, because a
   `frame_type`-less capture writes a flat `<uuid8>.fits` beside the
   templated tree, and frames from an earlier, shallower
   `directory_pattern` stay reachable too. For each candidate, verify
   by reading the FITS header `DOC_ID` against the requested full
   UUID. The sidecar's `id` field is the fallback authority if the
   FITS is unreadable. On match, read both files, populate the cache,
   and return the entry.
3. **Not found.** Return `None`. The HTTP API returns `404`.

Ghost-match disambiguation runs only when multiple files in the data
directory share an 8-char suffix. With UUID v4 entropy, the expected
number of ghost matches per query is `k/N` where `k` is the total
captures on disk and `N = 2^32`. At 100,000 captures, that's ~2·10⁻⁵
— the disambiguation path exists for correctness but in practice
essentially never fires.

If `<data_directory>` is changed at runtime (rare), entries captured
under the old directory become unreachable by id even though their
files remain on disk. This is intentional — the data directory is a
contract, not an indexed pool. Operators wanting to bring old captures
back into reach copy or move the relevant FITS+sidecar pairs into the
current `<data_directory>`.

#### Storage Type Selection (u16 vs i32)

The cache primarily stores **`u16`**. All current consumer/prosumer
astro cameras (ZWO ASI series, QHY, Atik, Moravian, SBIG) emit
non-negative pixel values within the 16-bit range — CCDs are uniformly
16-bit; CMOS is 12-, 14-, or 16-bit ADC; sensor output is a
photoelectron count, physically non-negative. Storing `u16` halves
cache memory and `/pixels` bandwidth versus `i32` at no information
loss for any camera in this category.

The `CachedPixels::I32` variant exists so the structure can accept
future scientific cameras (Andor, Hamamatsu sCMOS HDR modes, etc.)
that genuinely emit values outside `u16` range, without a refactor.

Selection policy at `capture` time:

- Read `max_adu` from the cached `CameraEntry.max_adu` populated by
  `connect_camera` (one Alpaca round-trip per connection, not per
  exposure — see "Tenet 1: don't re-fetch invariant data"). The
  cached value drives both the cache variant choice and the `max_adu`
  field on the resulting `ExposureDocument`.
- If `max_adu ≤ 65535`: narrow the i32 array returned by
  `ascom-alpaca` to `u16` and store as `CachedPixels::U16`. The
  narrow clamps to `[0, max_adu]` before casting — guards against a
  buggy driver returning out-of-range values.
- Otherwise: store as `CachedPixels::I32` unchanged.
- If `max_adu` is `None` on the entry (connect-time read failed): skip
  the cache insert and persist `max_adu: None` on the document. The
  FITS file plus the sidecar remain the durable record. The next
  reconnect re-reads independently.

The decision is per-capture in mechanism (consulted at every
exposure) but the underlying value is per-connection — `MaxADU` is a
physical property of the sensor and cannot change while the device
stays connected. A connect-time read failure therefore degrades the
whole session for that camera until reconnect, in exchange for cutting
five Alpaca property round-trips out of every capture (mitigates the
load pattern that triggers OmniSim's per-capture `GC.Collect` ↔
telescope-timer-thread race).

On disk-fallback rehydration (cache miss, document/pixels read from
the FITS+sidecar pair), the variant choice comes from the sidecar's
`max_adu` field — no live camera required. If the sidecar's
`max_adu` is null (capture-time read failed), the rehydration falls
back to serving from disk for each request rather than caching.

Analysis code is generic over the pixel type via a small trait
(e.g. `Pixel: Copy + Into<i64> + ...`) implemented for both `u16` and
`i32`. Each algorithm is written once, monomorphized for both types.
Tools dispatch:

```rust
match &cached.pixels {
    CachedPixels::U16(arr) => measure_basic_impl(arr.view()),
    CachedPixels::I32(arr) => measure_basic_impl(arr.view()),
}
```

FITS writes preserve the cache pixel type: 16-bit sensors land on
disk as BITPIX=16+BZERO=32768 (half the byte cost of the previous
BITPIX=32 widening); cameras with `max_adu > u16::MAX` fall through
to BITPIX=32 (lossless). Reads always normalise to `i32` — the
imaging pipeline is uniform regardless of on-disk bit depth. The
ASCOM `ImageArray` interface contract — which mandates `Int32` — is
honored at any point we surface pixels through that API; internally
we use `u16` whenever possible.

#### HTTP API (services and plugins)

| Endpoint | Returns | Description |
|----------|---------|-------------|
| `GET /api/documents/{document_id}` | JSON | Full exposure document with all sections. Resolves through the cache (hit → return; miss → disk fallback; not found → 404). See [Document Resolution](#document-resolution). |
| `POST /api/documents/{document_id}/sections` | — | Plugin section update. Requires the document be resolvable; persists the sidecar atomically and updates the cached entry. |
| `GET /api/images/{document_id}` | JSON metadata | Width, height, bitpix, FITS path, exposure document link, in-cache flag. Resolves through the same cache + disk fallback. |
| `GET /api/images/{document_id}/pixels` | `application/imagebytes` | Raw pixel data in [ASCOM Alpaca ImageBytes](https://ascom-standards.org/api/) format: 44-byte header (metadata version, error number, transaction IDs, data offset, image element type, transmission element type, rank, dimensions) followed by little-endian pixel bytes. |

Symmetry: `/pixels` serves the same wire format Alpaca cameras produce
upstream. A plugin that already speaks Alpaca can reuse its existing
ImageBytes parser unchanged.

There is deliberately **no FITS endpoint**. Consumers that genuinely
need FITS-formatted bytes (typically the plate-solver service, since
ASTAP and astrometry.net are FITS-native) read the file directly from
the path in the exposure document — `rp` and its plugins/services are
assumed to share a filesystem (see [File Accessibility](#file-accessibility)).
HTTP-proxying a file consumers can already open is unnecessary overhead.

#### Lifetime and Eviction

- **Insertion**: on `capture` completion, after the FITS+sidecar pair
  is written. The cache holds the pixel buffer that came from the
  camera plus the freshly-constructed document — no re-decode or
  re-parse at insert time.
- **Eviction**: LRU. Pixels and document are evicted together as a
  unit. Two configurable budgets, whichever trips first:
  ```json
  "imaging": {
    "cache_max_mib": 1024,
    "cache_max_images": 8
  }
  ```
  `cache_max_mib` bounds the **combined** memory footprint of pixels
  and serialized document JSON for each entry. Document size is not
  negligible — analysis sections like `detect_stars` and
  `measure_stars` carry per-star arrays that can run into tens of KB
  per section. `cache_max_images` is a safety net against
  misconfiguration. Defaults are sized for an 8 GB Pi 5; tune for
  larger hosts.
- **Fallback**: cache miss is not an error. Tools and the
  document/image HTTP endpoints fall back to the on-disk pair via
  [Document Resolution](#document-resolution). After successful
  rehydration the entry is re-inserted into the cache.
- **Durability**: a document remains accessible by id as long as its
  FITS+sidecar pair sits in `<data_directory>`, regardless of cache
  state or `rp` restart history. The contract is "live as long as the
  file is on disk", not "live as long as `rp` is up".

#### Wire Format Choice

ImageBytes was chosen over a custom format or NumPy `.npy` because:
- It's the format the camera already produced; same parser code is
  reusable by plugins that already consume Alpaca devices directly.
- The 44-byte header carries everything we need (rank, dimensions,
  element type) without ad-hoc HTTP headers.
- It's a published ASCOM standard — no rp-specific format to document.
- It's **type-tagged**, which lets the `/pixels` endpoint honestly
  reflect the cached storage type in the header
  (`ImageElementType=UInt16` for `CachedPixels::U16`,
  `ImageElementType=Int32` for `CachedPixels::I32`). Consumers parse
  the header and handle the type — no client-side assumption baked
  in. This means a future Andor / Hamamatsu integration that bumps
  the cache to `I32` for those frames is a transparent wire change,
  not an API break.

### Plugin-Provided Tools

Tool-provider plugins extend the catalog with tools `rp` does not ship
built-in. A plugin runs its own MCP server. At startup, `rp` connects to
each tool-providing plugin as an MCP client, discovers their tools via
`tools/list`, and proxies them through its own MCP server. Orchestrators
and other clients see a single unified catalog — they don't know or care
whether a tool is built-in, an rp-managed service proxy, or a plugin
contribution.

Tool-provider plugins are typically third-party: experimental algorithms,
ML-based analyzers, alternative implementations of an existing tool that
a specific deployment wants to run alongside the built-in, or anything
written in a non-Rust language. Stable astronomy primitives (HFR, FWHM,
eccentricity, V-curve focus, iterative centering, plate-solve proxy)
ship as built-ins. A provider's tool must carry a name no built-in and
no other provider uses — there is no shadowing; see
[Config-Time Validation](#config-time-validation) and
[Third-party alternatives](#third-party-alternatives).

(Orchestrators such as `session-runner` and `polar-align` are not
plugins: they *consume* tools as MCP clients and `rp` registers nothing
for them — see [Plugin Types](#plugin-types). `calibrator-flats` is
the first first-party tool provider: it serves `train_flats`,
`take_flats` and `get_flat_training` through this catalog and drives
the rig by calling `rp` back — [calibrator-flats.md](calibrator-flats.md).)

```
┌─────────────────┐  tools/list   ┌──────────────────┐
│  star-analyzer   ├─────────────►│                  │
│  (MCP server)    │              │       rp         │  tools/list + tools/call
│  measure_eccen.. │◄─────────────┤  (MCP server +   ├──────────────────────────►  workflow plugins
└─────────────────┘  tools/call   │   MCP client)    │                             (MCP clients)
                                  │                  │
┌─────────────────┐  tools/list   │  Aggregates all  │
│  wavefront-anlzr ├─────────────►│  tools into one  │
│  (MCP server)    │              │  unified catalog  │
│  measure_wavefr..│◄─────────────┤                  │
└─────────────────┘  tools/call   └──────────────────┘
```

Examples of genuinely third-party-shaped plugins (none of these ship
with `rp`):

| Tool | Provider | Description |
|------|----------|-------------|
| `classify_image_quality` | ml-quality-classifier | ML model that scores frames as keep/reject |
| `detect_diffraction_pattern` | bahtinov-mask-helper | Specialized analyzer for Bahtinov / tri-Bahtinov focus aids |
| `measure_wavefront` | wavefront-analyzer | Optical aberration analysis from defocused star images |
| `score_field_flatness` | tilt-analyzer | Detect sensor tilt by per-quadrant HFR comparison |

**All tool results that produce image metrics MUST be written into the
exposure document as a section.** This is the one rule — the document is
the shared data bus. `rp` enforces this: compute tool results are merged
into the document before being returned to the caller.

#### How a proxied call behaves

`rp` dials each provider at startup through the standard client
(`rp-mcp-client`, [ADR-017](../decisions/017-standard-mcp-client-construction.md)
— the same credential policy every first-party client follows: the
registration's `auth` is presented as HTTP Basic over verified HTTPS
only, trusting the top-level `ca_cert`), runs `tools/list`, and adds
one proxy route per discovered tool under the provider's own `Tool`
record (name, description, input and output schemas, annotations),
unchanged. From then on:

- **Forwarding.** A `tools/call` for a provider tool is forwarded with
  the caller's arguments and `_meta` (minus the protocol-reserved
  `io.modelcontextprotocol/*` keys, which the client transport sets
  itself, and `progressToken`, which is replaced by one `rp` mints for
  the forwarded request). The provider's result comes back
  **verbatim**: a tool error stays a tool error, and a JSON-RPC error
  from the provider (invalid params, its own refusal) is relayed as
  that JSON-RPC error.
- **Progress.** Every `notifications/progress` the provider emits for
  the forwarded request is re-emitted to the caller under the caller's
  own `progressToken`, in order and before the result; a caller that
  sent no token gets none.
- **Safety.** A proxied call is entered in the in-flight registry like
  a built-in (§ [In-Flight Tool Calls](#in-flight-tool-calls)) with the
  class its registration gives it (§ [Tool Provider
  Registration](#tool-provider-registration)): a gated provider tool is
  refused with `SafetyUnsafe` while conditions are unsafe, and when its
  `Cancel` fires — the unsafe transition, or the caller going away —
  `rp` sends `notifications/cancelled` for the provider's request and
  answers the caller `cancelled: <reason>`.
- **Outage.** The catalog is built once and stays stable. A provider
  that stops answering is marked unreachable on the first failed call
  (`provider_changed`, `connected: false`, once per transition); its
  tools stay in the catalog and answer a tool error naming it —
  `` tool provider `<name>` is unreachable: … `` — until the reconnect
  supervisor's provider lane (§ [Device Session
  Recovery](#device-session-recovery)) re-dials it on
  `equipment.reconnect_interval` and emits `provider_changed` with
  `connected: true`. There is no re-discovery on reconnect: a provider
  whose tool set changed needs an `rp` restart, which the error message
  says and a supervisor warning names tool by tool.

### Plugin Types

Plugins are separate processes following the plugin protocol. Some are
first-party; others are third-party extensions. Two plugin types by
role:

| Type | Role | Interface | Typical authorship |
|------|------|-----------|-------------------|
| **Event** | React to events asynchronously | Webhook (receive events, post completion) | Either |
| **Tool provider** | Add tools beyond `rp`'s built-in catalog | MCP server (rp aggregates their tools) | Mostly third-party |

A plugin can combine types. For example, a focus plugin can be a
**tool provider** (exposes an `auto_focus_ml` tool beside the built-in
`auto_focus`) and also an **event plugin** (subscribes to
`temperature_changed` to track focus drift).

**Orchestrators are not a plugin type.** The client that drives the
imaging session (`session-runner`, `polar-align`) is an MCP client that
starts itself — an operator posts to its `/runs` — and `rp` needs no
registration to serve it, so it keeps none. (`calibrator-flats` left
this list when it became a tool provider — [calibrator-flats.md](calibrator-flats.md).) A
`plugins[]` entry with `"type": "orchestrator"` is rejected at config
load, by `PUT /api/config` and by `rp doctor` with a message naming
the migration (orchestrator registrations were removed; start runs at
`session-runner` — [`mcp-sessionless.md`](../plans/mcp-sessionless.md)
D11), as is a `session.session_state_file` key: silently accepting
either would leave an operator believing `rp` will start their session
at dusk. `rusty-photon-doctor` reports the same on an installed config
as `rp.orchestrator-registration-removed`.

#### Tool Provider Registration

Tool providers run their own MCP servers. `rp` connects at startup,
discovers their tools, and proxies them through its own MCP server:

```json
{
  "name": "ml-quality-classifier",
  "type": "tool_provider",
  "mcp_server_url": "http://localhost:11150/mcp",
  "requires_tools": ["compute_image_stats"]
}
```

`mcp_server_url` is required and must be an `http://` or `https://`
URL (a bad scheme or an embedded credential is rejected at load, like
an event plugin's `webhook_url`); `name` is required and must be unique
among tool providers, because it is how a proxied tool's errors, the
`provider_changed` event and the startup log identify the provider.
`auth`, when present, is the observatory credential
(`{"username", "password"}`, the shape every first-party client
presents — [ADR-017](../decisions/017-standard-mcp-client-construction.md));
`rusty-photon-doctor` joins `mcp_server_url` and `auth` against the
provider's own server config like every other client target
([doctor.md § Client-target joins](doctor.md#client-target-joins)).

The `requires_tools` field is for startup validation only — `rp` checks
that every listed tool exists in the merged catalog (built-ins plus
every provider's tools) before serving, and refuses to start naming the
missing ones otherwise. At runtime, the plugin can call any tool on
`rp`.

A provider that cannot be reached at startup — three attempts a second
apart, each bounded to 10 s, the same shape as the device connect
budget — fails startup naming it: without its `tools/list` there is no
catalog to build, and a catalog that silently lacked the provider's
tools would fail the night's first document instead. A provider that
goes away *after* startup is an outage, not a config fault (§
[Plugin-Provided Tools](#plugin-provided-tools)).

A provider's tools are **gated** by default (§ Safety → [In-Flight Tool
Calls](#in-flight-tool-calls)): `rp` cannot know what a foreign tool
moves, so while conditions are unsafe they are refused with
`SafetyUnsafe` and cancelled on the unsafe transition like a built-in
slew. A registration opts a tool out per name with `"gate": "none"`:

```json
{
  "name": "ml-quality-classifier",
  "type": "tool_provider",
  "mcp_server_url": "http://localhost:11150/mcp",
  "gate": { "classify_image_quality": "none" }
}
```

The shipped provider, `calibrator-flats`
([calibrator-flats.md](calibrator-flats.md)), registers with its three
tools opted out — flats run behind a closed cover, and the only tool
that exposes the optics is `rp`'s own gated `open_cover` — and with the
`rp` tools it calls as its dependency list. A fresh install without this
entry has no flats tools; one without the `gate` map has them gated:

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

The packaged `rusty-photon-rp.service` orders itself `After=` the
provider's unit so a cold boot finds it up before `rp` dials it.

A `gate` key naming a tool the provider does not offer fails startup.
The operator's `safety.gate` overrides (§ Configuration) apply on top,
so a deployment can still move a provider tool either way: a
`safety.gate` entry naming a provider tool wins over the registration's
own `gate` key. Because a provider's tools are only known once it has
been dialed, the config loader (and `PUT /api/config`, and `rp doctor`)
can only check a `safety.gate` name against the built-in catalog — with
at least one tool provider registered, a name that is not a built-in is
deferred to startup, where it is checked against the merged catalog and
rejected, naming the entry, if it is in neither.

#### Example: ML Quality Classifier (third-party tool provider)

A third party ships an ML model that scores frames as keep/reject. It
runs as a separate process, exposes one tool, and reads pixels from
the image cache:

```
Orchestrator calls: tools/call classify_image_quality {document_id: "doc-042"}
  → rp proxies to ml-quality-classifier's MCP server

  ml-quality-classifier (in its own process):
    → GET /api/images/doc-042/pixels  (Alpaca ImageBytes)
    ← raw pixel bytes
    → runs inference locally
    → POST /api/documents/doc-042/sections {section_name: "ml_quality", data: {...}}

  ml-quality-classifier returns to rp:
    ← {score: 0.87, classification: "keep", model: "psf-cnn-v3"}

  rp returns to orchestrator:
    ← {score: 0.87, classification: "keep", model: "psf-cnn-v3"}
```

The plugin reuses `rp`'s image cache HTTP API for pixel access (no FITS
re-decode), and writes its results back into the exposure document via
the section endpoint. Built-in compound tools (`auto_focus`,
`center_on_target`) follow the same orchestration pattern but without
the MCP-over-HTTP hop — see [Compound Tools](#compound-tools).

### Safety Guardrails

There is no per-workflow scoping — any workflow plugin can call any tool
in the catalog. Safety is enforced at the tool level, universally:

- **Parameter validation**: focuser position within min/max bounds,
  exposure duration within configured limits, slew coordinates above
  horizon.
- **State validation**: cannot capture while another capture is in
  progress on the same camera, cannot slew during an exposure.
- **Safety override**: a safety event (unsafe transition) immediately
  cancels every in-flight gated tool call (§ Safety → [In-Flight Tool
  Calls](#in-flight-tool-calls)) — the caller sees the tool error
  `cancelled: safety` — and every gated tool called while conditions
  remain unsafe is refused with the `SafetyUnsafe` JSON-RPC error.
  Ungated calls already in flight complete, except a `capture`, whose
  exposure the transition aborts through the same path, and ungated
  tools keep answering throughout. `rp` ends nothing and resumes
  nobody: on the safe transition it lifts the gate and emits
  `safety_changed`, and the orchestrator resumes its own run (see
  § Safety).

### Config-Time Validation

At startup, `rp` validates the full plugin dependency graph:

1. Connect to each tool-providing plugin's MCP server and discover
   their tools via `tools/list`. A provider that cannot be reached
   within the connect budget fails startup, naming it.
2. Build the unified tool catalog from built-in tools and all
   discovered plugin-provided tools. A tool name offered by a plugin
   *and* built into `rp`, or by two plugins, is a hard error naming
   both sources (`rp` refuses to start) — there is no shadowing and no
   precedence between plugins (tenet 2: a catalog whose entries depend
   on which registration came first is a config fault waiting for
   2 a.m.). A site that wants an alternative to a built-in ships it
   under its own name (§ [Third-party
   alternatives](#third-party-alternatives)).
3. For each plugin with `requires_tools`, verify that every listed
   tool exists in the merged catalog.
4. If validation fails, `rp` refuses to start and reports the missing
   or conflicting tools.

This ensures the system is fully configured before the session begins.
A missing dependency is a startup error, not a 3 AM surprise.

## Equipment Integration

### ASCOM Alpaca Devices

All devices with an Alpaca interface are accessed exclusively via ASCOM Alpaca
HTTP API. `rp` is an Alpaca client, not a server. Equipment is
configured in the JSON config file — no discovery protocol is used.

Supported ASCOM device types:

| Device Type | Usage |
|-------------|-------|
| Camera | Exposure control (start, abort, readout status, cooler) |
| Telescope (mount) | Slew, track, park, unpark, side of pier, meridian flip |
| Focuser | Absolute/relative move, temperature readout |
| FilterWheel | Filter selection by position |
| SafetyMonitor | Safety state polling |
| CoverCalibrator | Dust cover control (open, close) and flat panel control (on, off, brightness); train-addressable as the first device of an optical train |
| Switch | Roster membership and connectivity status only — no MCP tool integration yet |
| Rotator | Absolute sky-angle move + position readback (`move_rotator`, `get_rotator_position`); train-addressable |
| ObservingConditions | Roster membership and connectivity status only — no MCP tool integration yet |
| Dome | Roster membership and connectivity status only — no MCP tool integration yet |

**Mount site properties.** On telescope connect, `rp` reads
`SiteLatitude` and `SiteLongitude` to validate the configured `site`
block. A mismatch greater than `0.01°` in either dimension is a
hard error; if either read fails (typically `NOT_IMPLEMENTED` from
a mount that does not expose the property), the validation is
skipped with a `debug!()` log. See
[Site Validation Against the ASCOM Mount](#site-validation-against-the-ascom-mount).

**CA trust for `https://` devices.** An `alpaca_url` may be `https://`
when the device's own service is TLS-enabled (e.g. via doctor's D6
provisioning). `rp`'s Alpaca client verifies that certificate against
the top-level `ca_cert` config (see [Configuration](#configuration)) —
an observatory runs one self-signed CA, so one rp-level setting covers
every device, the plate-solver service, the guider service, and the
event plugins alike.
Setting `ca_cert` makes it the client's **only** trusted root
(`tls_certs_only`, ADR-002): the platform trust store no longer
applies, so a public-CA `https://` target becomes unreachable
alongside the observatory CA. Without `ca_cert` set, an `https://`
device signed by that CA fails certificate verification regardless of
per-device `auth` credentials.

### Device Session Recovery

An Alpaca device session is server-side state: `Connected = true` lives
in the downstream service's memory, so a restart of that service
silently resets it while `rp`'s client handle stays valid. Without
recovery, every subsequent call fails `NOT_CONNECTED` until `rp` itself
restarts. Three situations produce a dead session:

1. **An established session goes stale** — the downstream service
   restarted (crash, package upgrade, or a Sentinel recovery restart).
2. **The device was unreachable at `rp` startup** — the connect retry
   budget (3 attempts over ~3 s) elapsed and the entry was registered
   disconnected. On a cold boot systemd starts the fleet in parallel,
   so `rp` racing a device service to readiness is an ordering roll of
   the dice, not an edge case.
3. **The device's service was reachable but its roster did not contain
   the configured device yet** while the service was still
   initializing — the "device not found at index" outcome.

A background reconnect supervisor heals all three without an `rp`
restart. Every `equipment.reconnect_interval` (humantime string,
default `"30s"`; must be greater than zero — a zero interval would be
a busy loop and is rejected at config load) it walks the configured
devices:

- **Health check.** For an entry whose session is marked live, read the
  Alpaca `Connected` property. `true` ⇒ healthy, nothing else happens.
- **Re-establish.** For an entry reporting `Connected = false`, failing
  the health read, or already marked disconnected, run the full connect
  routine: re-enumerate the server's device roster, re-issue
  `Connected = true`, and re-read the connect-time property cache (a
  camera's `MaxADU`, pixel pitch, sensor geometry). Nothing is carried
  over from the dead session — the service may have come back with a
  different device behind the same config entry, so nothing is assumed.
  This holds even when some other client turned the device back on in
  the meantime: rp never adopts a session it did not establish, so the
  property cache is always the establish routine's own fresh read.
- **On success** the new session replaces the old one, the entry's
  `connected` flag turns true, and an `equipment_changed` event is
  emitted. The event fires on every successful re-establishment — also
  when the flag never observably flipped (a service bounce between two
  supervisor passes) — so a healed session is always visible in the
  event stream.
- **On failure** the entry is marked disconnected (`equipment_changed`
  once per transition, not once per attempt) and the next pass retries.
  There is no give-up state: an outcome that is permanent within one
  connect routine ("device not found") is still retried on the next
  pass, which is exactly what case 3 needs.

The cadence is fixed — no exponential backoff. One `Connected` read per
device per interval is the steady-state cost, and the interval itself
bounds the load on an unreachable host. A pass handles devices
sequentially and the loop sleeps for the full interval between passes,
so worst-case recovery latency is one interval plus the pass itself —
the connect routines of every other device needing re-establishment (a
few seconds each, bounded by the connect retry budget) run ahead of the
last device's.

Consequences and constraints:

- **Fail-safe behavior is unchanged.** Until recovery succeeds, a
  safety monitor still reads as unsafe and a device call still errors.
  Recovery is an availability mechanism, not a safety mechanism.
- **Tenet 3 applies to the reconnect path exactly as to first
  connect:** re-establishing a session re-*reads* state and never
  re-commands hardware. `Connected = true` is non-actuating by driver
  contract.
- **In-flight calls are unaffected.** A tool call holding the old
  session handle keeps using it (the transport is stateless HTTP). A
  disconnected entry keeps its stale handle until a successful
  re-establish replaces it, so concurrent callers see honest
  `NOT_CONNECTED` errors rather than a mid-operation handle swap.
- `rp` never issues `Connected = false`, so the supervisor cannot fight
  an intentional disconnect — there is none.
- `GET /api/equipment`'s `connected` flags reflect this live state, not
  the startup snapshot (see [Equipment](#equipment)).
- The Sentinel watchdog's [Recovery Flow](#recovery-flow) ends with
  "notify `rp` to reconnect". The supervisor makes that step pull-based
  and self-contained: `rp` notices and reconnects on its own cadence
  regardless of who restarted the service, and no notify endpoint is
  required.

**The tool-provider lane.** The same pass, after the devices, walks
every registered tool provider (§ [Plugin-Provided
Tools](#plugin-provided-tools)): a live session is health-checked with
`tools/list` (bounded to 10 s), a dead one — the health check failed,
or a proxied call already marked it unreachable — is re-dialed with
the full `server/discover` bootstrap. Success installs the new session
and emits `provider_changed` with `connected: true`; a failed health
check emits `connected: false` once per transition and the next pass
retries. Nothing is re-discovered: the catalog keeps the tools found
at startup, and a live provider whose `tools/list` no longer matches
them is logged at `warn!` naming what was added and removed — restart
`rp` to pick the change up. A proxied call holding the old client
keeps using it, as an in-flight device call keeps its handle.

### Optical Trains

`equipment.optical_trains` models each camera's light path as an
ordered list of roster device ids, objective side first, terminating
in a camera. Membership expresses coupling, position expresses optical
order, and rp derives focus pairing and ordering, rotation effects,
and the guider's focus dependency from the lists instead of being told
each pairing per workflow. The design rationale and the phasing of the
consumers live in
[`docs/plans/optical-trains.md`](../plans/optical-trains.md); the
decisions recorded there are fixed.

```jsonc
"optical_trains": [
  { "id": "main",  "purpose": "imaging", "focal_length_mm": 1000.0,
    "default_position_angle_degrees": 254.0,
    "devices": ["flat-panel", "main-focuser", "main-fw", "falcon", "main-cam"],
    "auto_focus": { "duration": "3s", "step_size": 100, "half_width": 1000,
                    "min_area": 4, "max_area": 500 } },
  { "id": "guide", "purpose": "guiding", "focal_length_mm": 200.0,
    "devices": ["main-focuser", "guide-focuser", "guide-cam"],
    "auto_focus": { "step_size": 50, "half_width": 500,
                    "frames_per_step": 3 } }
]
```

Semantics:

- `devices` entries are roster ids from `equipment.cameras[]`,
  `equipment.focusers[]`, `equipment.rotators[]`,
  `equipment.filter_wheels[]`, and `equipment.cover_calibrators[]` —
  active devices only; passive optics (OAG bodies, reducers,
  flatteners) are not modeled. A device that physically affects
  several cameras (a drawtube focuser in front of an off-axis
  pick-off, a filter drawer in front of it) appears in several trains.
- A **cover calibrator** sits at the objective, so it may only be the
  **first** entry, and a train holds **at most one**: a rig with a
  motorized dust cap *and* a separate light panel (two CoverCalibrator
  devices, one cover-only, one calibrator-only) is not modeled — the
  error says so (calibrator-flats-provider plan, D3 and O1). One
  calibrator may be first in several trains: a flip-flat over the OTA
  covers the main camera and the OAG guide camera alike, and the
  merged-order rule below holds because it is first everywhere. The
  calibrator tools accept `train_id` and report the trains a cover or
  panel affects — see
  [CoverCalibrator Tool Details](#covercalibrator-tool-details).
- `purpose` is `"imaging"` (the default when omitted) or `"guiding"`.
  The guiding train tells rp which camera's focus and rotation state
  the guider depends on; at most one train may carry it, and it
  requires `equipment.mount.guiding`.
- `focal_length_mm` is the effective focal length of that light path
  in millimetres — a positive finite number, rejected at load
  otherwise. Optional: omitted, captures through that train's camera
  carry no `optics` block, exactly like a camera outside any train.
- `default_position_angle_degrees` is the train's default framing
  angle in degrees east of north, sky frame — the same domain as
  `move_rotator`'s `angle` (`0.0 ≤ angle < 360.0`, finite), rejected
  at load otherwise. Optional. It is layer two of the three-layer
  effective position angle (target value → this default → `0.0`
  north-up; [Target Store → Position angle](#position-angle), plan
  Decision 5): for a rotator-less train it documents the camera's
  fixed mounting angle as a physical fact of that light path — set it
  once, dial the same angle into the planetarium's FOV indicator, and
  frames match. Resolution happens at read time by design, so
  re-mounting the camera (and updating this value) deliberately
  reinterprets every target that inherits the default; per-target
  explicit angles freeze framing and are never reinterpreted.
- `auto_focus` is an optional per-train block holding the V-curve
  sweep parameters for focusing this train. Which fields it takes
  depends on the train's `purpose`, validated at load with
  dotted-path errors:
  - **imaging** trains run the capture sweep: `duration`,
    `step_size`, `half_width`, `min_area`, `max_area` (all required
    when the block is present) plus optional `threshold_sigma`
    (default `5.0`) and `min_fit_points` (default `5`).
  - the **guiding** train runs the PHD2-metric sweep: `step_size`
    and `half_width` (required) plus optional `frames_per_step`
    (default `3`) and `min_fit_points`. The capture-only fields
    (`duration`, `min_area`, `max_area`, `threshold_sigma`) are
    rejected in a guiding train's block, as is `frames_per_step` in
    an imaging train's — a knob that cannot influence the sweep
    must not pretend to.

  `step_size`, `half_width`, and `frames_per_step` must be positive
  integers, rejected at load otherwise. The block backs
  train-addressed `auto_focus` calls (per-call parameters override it
  field by field) and is required on every train a `refocus_train`
  expansion runs in — sweep geometry is per-train, which is exactly
  why it lives here and not in the tool call.
- Trains attach implicitly to the singular `equipment.mount`. Devices
  left out of every train stay legal and behave exactly as today —
  trains are enrichment, not a gate.

Validation happens at load and on `PUT /api/config`. Per-field
invariants (the `purpose` enum, `focal_length_mm` positivity) are
enforced in the field types at deserialize; the cross-array graph
rules run in the shared `validate_config` pass, reporting dotted
`FieldError` paths (`equipment.optical_trains.0.devices.2`) that name
the offending entry:

- train ids are unique;
- every `devices` entry exists in the roster as a camera, focuser,
  rotator, filter wheel, or cover calibrator; no id repeats within one
  train;
- the last entry is a camera, cameras appear nowhere but last, and a
  camera terminates at most one train;
- a cover calibrator appears nowhere but first (the error names the
  position), and a train contains at most one (the error names both
  ids and says the two-device rig is not modeled);
- devices shared between trains appear in a consistent relative order
  across them (the merged order relation is acyclic);
- at most one train has `purpose: "guiding"`, and a guiding train
  requires `equipment.mount.guiding`.

Derivation rules — the questions the derived train model answers.
Consumers land phase by phase per the plan:

| Question | Rule |
|---|---|
| Which focuser focuses camera C? | Last focuser in C's train list |
| AF sequence after a refocus trigger on train T | Shared focusers of T upstream-first (each run in the train where it is terminal), then T's terminal focuser |
| What does moving focuser F invalidate? | Focus of every train containing F |
| What does rotator R rotate? | Every train containing R (when one is the guiding train and guiding is active, `move_rotator` runs the rotate-while-guiding ladder — see [Rotator Tool Details](#rotator-tool-details)) |
| What does a filter change on wheel W invalidate? | Focus offset of trains containing W (per-filter offsets: backlog) |
| What does cover calibrator C cover or light? | Every train containing C — reported as `trains` on every calibrator tool result |
| What is in train T? | `get_train_info`: the terminal camera, the sole filter wheel with its filter names, the calibrator, the focusers, the sole rotator |
| Who is perturbed by dither/slew/flip? | Every train on the mount — serialized against imaging-train exposures by the [mount motion gate](#mount-motion-gate) |
| Pixel-scale conversions | Train `focal_length_mm` + the camera's reported pixel size |

Consumers of the derived model:

- The exposure document's `optics` block
  ([Core Fields](#core-fields-owned-by-rp)): capture resolves
  `focal_length_mm` through the captured camera's train.
- `auto_focus` accepts `train_id` as an alternative to the explicit
  `camera_id` + `focuser_id` pair, resolving the train's camera and
  terminal focuser and falling back to the train's `auto_focus`
  config block for sweep parameters — see the
  [`auto_focus` Contract](#auto_focus-contract).
- `capture` and `center_on_target` accept `train_id` as an
  alternative to `camera_id` (the train's terminal camera), and
  `set_filter` as an alternative to `filter_wheel_id` (the train's
  sole filter wheel — none or several is an error naming the train).
  Device-id addressing stays first-class on every train-addressable
  tool; trains are an alternative spelling, not a replacement. This
  is what lets a workflow document take a single `train_id`
  parameter — the shipped `deep_sky.json` does
  (session-runner.md § `deep_sky.json`). Every train-addressable
  tool's input schema publishes its alternatives as a top-level
  `oneOf` of **presence-only** branches (each carrying nothing but
  `required`, e.g. `[{"required": ["camera_id"]}, {"required":
  ["train_id"]}]`; `refocus_train` simply marks `train_id` required)
  so schema-driven validators — session-runner's layer-2 catalog
  validation — can fail a call that names no alternative, or
  several, before anything moves.
- `refocus_train` expands one trigger into the dependency-ordered AF
  sequence, with a guiding pause/resume handshake around steps that
  move a guiding-train focuser — see the
  [`refocus_train` Contract](#refocus_train-contract).
- The rotator tools (`move_rotator`, `get_rotator_position`) accept
  `train_id` addressing and report which trains a move rotated — see
  [Rotator Tool Details](#rotator-tool-details).
- The calibrator tools (`get_cover_state`, `open_cover`,
  `close_cover`, `calibrator_on`, `calibrator_off`) accept `train_id`
  addressing (the train's cover calibrator) and report `trains`, every
  train the cover or panel affects — see
  [CoverCalibrator Tool Details](#covercalibrator-tool-details).
- `get_train_info` describes a train's resolved members without
  touching a device, so a tool provider addressed by `train_id` (the
  calibrator-flats-provider plan) can learn the camera to read, the
  wheel's filter names and the calibrator to drive while `rp` stays
  the only owner of the train model.
- `dither` converts `main_px` / `arcsec` amounts to guide-camera
  pixels via train pixel scales (train `focal_length_mm` + the
  camera's connect-time pixel size) — see the note under the Guider
  tool table.
- The [mount motion gate](#mount-motion-gate) admits captures as
  shared holders based on train membership: only captures through a
  camera terminating an imaging train contend with mount motion.

Auto-focus **on the guiding train itself** never captures through
the guide camera (PHD2 may own it at the SDK level): train-addressed
`auto_focus` and `refocus_train` steps that run in the guiding train
use the PHD2-metric sweep — moving the focuser and reading PHD2's
per-frame HFD — and require an active guide loop. See the
[Guide-train sweep](#guide-train-sweep-phd2-metric-variant)
contract, the rotate-while-guiding ladder under
[Rotator Tool Details](#rotator-tool-details), and the
[Guide Focus Watch](#guide-focus-watch).

### Mount Motion Gate

Dither, slews, and meridian flips move every train on the mount, and
any of them ruins an in-flight exposure on any imaging train. The
motion gate is an rp-internal readers-writer gate on the singular
mount that serializes the two: mount motion takes the gate
**exclusively**, imaging-train exposures hold it **shared**. It has
no configuration surface — it is always on. Captures contend only
through imaging-train cameras, so a rig with no imaging trains
merely serializes motions against one another; and motions acquire
before resolving the mount, so even a slew that will fail for want
of a configured mount passes through the gate first.

Acquisition rules:

| Operation | Gate mode | Notes |
|---|---|---|
| `slew` — including `center_on_target`'s inner slews and orchestrator-driven meridian flips, which reach the mount as slews | Exclusive | Acquired before the pre-slew pointing read, so the predictive deadline never includes gate wait |
| `dither` | Exclusive | Acquired after parameter and unit resolution (invalid calls fail fast without waiting), before the proxy call to the guider service; held through settle. A dither cancelled mid-settle answers its caller at once but hands the permit to a detached holder that keeps the gate exclusive until the guider's settle RPC ends — bounded by the settle timeout plus 15 s, or 90 s when the call named none — so no capture starts into the tail of guide pulses |
| `capture` through a camera terminating an **imaging** train — including the internal captures of `auto_focus`, `refocus_train`, and `center_on_target` | Shared | Held for the full exposure-to-persistence pipeline; concurrent imaging-train captures share freely |

Queueing semantics (Decision 5 of the
[optical-trains plan](../plans/optical-trains.md)):

- A **pending exclusive blocks new shared acquires** — no
  starvation: in-flight subs complete, the motion runs and settles,
  held captures then start. The queue is fair FIFO, so queued
  motions run in arrival order and captures queued behind a motion
  start as soon as it releases.
- Waits are **transitively bounded** and the gate adds no timeout of
  its own: every holder is already deadline-bounded (captures by
  `duration` plus the readout backstop, slews and dithers by their
  own predictive deadlines and settle timeouts), so the longest
  possible wait is the sum of the holders' own ceilings. An
  **aborted** exposure releases much sooner than the backstop: the
  capture poll treats a camera back at `Idle` with no image (two
  consecutive reads, plus a final `ImageReady` re-check) as an
  aborted exposure and fails the capture promptly — otherwise the
  safety enforcer's `AbortExposure` would leave the shared permit
  held for the whole readout grace, blocking the recovery slew that
  follows a safety interruption.
- When an exclusive request cannot start immediately, rp emits the
  point event `mount_motion_pending {operation}` and then blocks.
  The operation's own `*_started` envelope is emitted only **after**
  the gate is acquired, so predictive deadlines never include gate
  wait; the pending event is what fills that observability gap.
  Shared acquires emit no pending event — `exposure_started`, also
  emitted post-acquire, tells that story.

Exemptions — operations that deliberately bypass the gate:

- **Guide pulses.** PHD2 corrects the mount directly; pulses are
  sub-arcsecond by design and are the one motion imaging coexists
  with.
- **`capture` through a camera outside any train, or in the guiding
  train.** Trains are enrichment, not a gate (plan Decision 10): a
  rig without trains behaves exactly as before this feature, and
  guide-camera frames are not imaging subs.
- **`park`.** Parking is a terminal or emergency action: the safety
  enforcer (which aborts in-flight exposures *first*, then parks
  directly against the device registry rather than through the MCP
  tools) and an operator's park must never queue behind a
  multi-minute sub. A park mid-exposure abandons the sub by intent.
- **`unpark`, `sync_mount`, `abort_slew`.** The first two involve no
  physical motion (`unpark` clears a flag, `sync_mount` re-labels
  coordinates), and `abort_slew` stops motion — it must never queue
  behind the very motion it is aborting.
- **`start_guiding` / `stop_guiding`.** Guiding lifecycle sequencing
  (calibration typically precedes imaging) is the orchestrator's
  concern; the gate stays out of it.

Focuser and rotator moves are not mount motion and take no part in
the gate: they perturb only the trains containing them, and the
compound tools that move them (`auto_focus`, `refocus_train`)
already sequence their own captures around the moves. Coordinating a
manual `move_rotator` against another train's exposure remains the
orchestrator's concern (the rotate-while-guiding ladder is plan
phase T4).

**Driver-internal flips.** The gate presumes rp is the sole source
of non-guiding mount motion. A driver that moves the mount on its
own schedule — concretely, star-adventurer-gti's opt-in
`flip_policy.auto_flip_during_tracking` — flips invisibly underneath
the gate and would trail any in-flight sub. Settled as prevention
over detection: driver-planned auto-flip **must stay disabled on
rp-orchestrated rigs**, which is both its shipped default and the
GTi design doc's stated posture (hosts like rp own flip timing
themselves via `SetSideOfPier`); rp does not subscribe to
`SideOfPier` changes or invalidate in-flight subs. A doctor
cross-check (warn when rp orchestrates a GTi mount whose config
enables auto-flip) is recorded as backlog in the optical-trains
plan. When rp grows scheduled flips of its own, they will be
rp-issued slews behind this gate like any other.

### Guider Service

The guider service is an **rp-managed service** that wraps PHD2 and
exposes an HTTP API to `rp`. The `phd2-guider` binary provides the
PHD2 JSON-RPC integration and runs as that HTTP service via its
`serve` mode — contract, error envelope, and supervision posture in
[`docs/services/phd2-guider.md`](phd2-guider.md) § "HTTP Service
Mode". Like the plate solver, it is a separate process because PHD2
itself is an external program with its own crash/restart behavior;
Sentinel can supervise and restart it via the standard
rp-managed-service flow. `rp` talks to it through the
`crates/rp-guider` HTTP client, configured by the optional
`equipment.mount.guiding` block (url, timeout, settle defaults, dither
amount, `recalibrate_above_deg`); the same client backs the safety
enforcer's stop-guiding-on-unsafe step. Guiding is mount-scoped by
construction — the guider corrects and dithers by moving the mount,
which moves every train on it — so the block lives inside
`equipment.mount` and cannot be configured without one. Like every
outbound client `rp` builds, it trusts the top-level `ca_cert` (see
[Configuration](#configuration)) for an `https://` `url`, and an
optional `auth` (`{username, password}`) sends HTTP Basic Auth
credentials to an auth-enabled guider service (issue #620); `doctor
--fix` wires it from the observatory credential once the guider
service's own `server.auth` is on (`joins.client-auth`,
`docs/services/doctor.md` §Client-target joins).

PHD2 uses JSON-RPC over TCP, which is the one exception to the Alpaca-only
rule — there is no Alpaca guider device type. The guider service encapsulates
this protocol so `rp` speaks only HTTP.

Guider operations are exposed as built-in MCP tools (`start_guiding`,
`stop_guiding`, `dither`, `pause_guiding`, `resume_guiding`,
`get_guiding_stats`). `rp` proxies these tool calls to the guider service's
HTTP API. This means workflow plugins (e.g., a meridian flip plugin) can
control guiding through the same MCP tool mechanism as any other equipment.
Swapping in a different guiding backend requires only a different guider
service that implements the same HTTP endpoints.

Beyond the tools, rp consumes four more guider-service endpoints
internally (phd2-guider.md § HTTP API): the per-frame **metrics**
window (the guide-train sweep and the
[Guide Focus Watch](#guide-focus-watch)), **equipment** (the
rotate-while-guiding ladder's rotator branch), **calibration/clear**
and **star/reselect** (the ladder's tail). When `start_guiding`
settles and the guiding train contains a rotator but PHD2 reports no
connected rotator, rp emits the point event
`guide_rotator_unmodeled` (plus a warning log): rotations of the
guide field will clear calibration above `recalibrate_above_deg`
instead of being angle-adjusted by PHD2 — connecting the rotator in
PHD2's profile is the better setup where the platform allows it.

### Guide Focus Watch

When `equipment.mount.guiding.focus_watch` is present, rp runs a
background watch over the guider service's per-frame star metrics
and turns a degrading HFD trend into **events, not actions**: the
orchestrator owns sequencing (§ Orchestration), so it decides when a
refocus fits between exposures. Both events carry the guiding
train's `train_id`, and the shipped `deep_sky.json` wires the
responses as triggers (session-runner.md § `deep_sky.json`): a
guide-only `auto_focus` on `guide_focus_degraded`, the full
`refocus_train` on `guide_focus_escalation`. rp never moves a
focuser on its own initiative.

Mechanics — the watch polls `GET /guiding/metrics` every
`poll_interval` (default `"5s"`) while the guider reports an active
guide loop, and is idle otherwise:

- **Baseline**: the median HFD of the first `window` (default 10)
  valid frames after guiding becomes active. The watch subscribes
  to rp's own event stream and re-arms after any `focus_complete`
  or `refocus_complete` that involved the guiding train **or moved
  one of its focusers** (a shared focuser swept in an imaging train
  changes the guide focus just the same) — a fresh focus is a fresh
  reference, and re-arming also clears the cooldown so a trend
  degrading against the new baseline fires immediately.
- **Degraded**: the median HFD of the trailing `window` frames
  exceeds `baseline × degrade_ratio` (default `1.25`). Emit
  `guide_focus_degraded {train_id, baseline_hfd, current_hfd, window}` once,
  then hold for `cooldown` (default `"10m"`) before the watch may
  fire again.
- **Escalation**: if the same degradation episode is still degraded
  `escalation_deadline` (default `"10m"`) after
  `guide_focus_degraded` fired — the guide-only AF the orchestrator
  ran did not recover the trend, or none ran — emit
  `guide_focus_escalation {train_id, baseline_hfd, current_hfd}` once per
  episode. The full `refocus_train` sequence is the indicated
  response: it covers the shared-focuser drift the guide-only sweep
  cannot fix.
- **Recovery** (trailing median back within the threshold) ends the
  episode silently; frames flagged `star_lost` or with a null HFD
  never enter a median.

All `focus_watch` fields are optional —
`{ "window": 10, "degrade_ratio": 1.25, "cooldown": "10m",
"escalation_deadline": "10m", "poll_interval": "5s" }` are the
defaults; `window` must be between 3 and 50 and `degrade_ratio` a
finite number > 1.0, rejected at load otherwise. The upper bound is the
guider service's per-frame metrics ring — 50 frames is the whole supply
the watch draws medians from, so a larger window could never be filled
and the watch would silently never fire. Omitting the block disables the
watch entirely.

### Plate Solver

The plate solver is an **rp-managed service** — a separate process that
wraps the operator-installed ASTAP CLI binary. The MCP tool surface
(`plate_solve`) is a built-in tool that proxies to the service; ASTAP
lives in the supervised wrapper process.

This shape (service rather than built-in Rust code) is chosen because:
- ASTAP is an external program `rp` cannot link against.
- It can hang or crash independently of `rp`.
- Sentinel can restart the wrapper via the standard rp-managed-service
  supervision flow (see [Sentinel Watchdog Integration](#sentinel-watchdog-integration)).

The plate solver can also subscribe to `exposure_complete` events for
background solving (deferred to v2; v1 is request/response only).

The choice of solver and the supervision posture are settled by
[ADR-005](../decisions/005-plate-solver.md). The service's own design
doc — HTTP contract, supervision contract, configuration, mock test
double — lives at [`docs/services/plate-solver.md`](plate-solver.md).
`rp`'s `crates/rp-plate-solver` client trusts the top-level `ca_cert`
(see [Configuration](#configuration)) for an `https://` `plate_solver.url`,
the same as every other outbound client `rp` builds. An optional
`plate_solver.auth` (`{username, password}`) sends HTTP Basic Auth
credentials to an auth-enabled plate-solver service (issue #620);
`doctor --fix` wires it from the observatory credential once the
plate-solver service's own `server.auth` is on (`joins.client-auth`,
`docs/services/doctor.md` §Client-target joins).
Implementation sequencing is in
[`docs/plans/archive/plate-solver.md`](../plans/archive/plate-solver.md).

### File Accessibility

Plugins and `rp` are assumed to share a filesystem (local paths
work). Distributed deployments where plugins run on separate machines are a
future concern and out of scope for the initial design.

## Camera Cooling

Dark frames only calibrate cleanly when lights and darks share the
sensor temperature, so a cooled camera must be regulated at a *defined*
temperature — not "wherever the cooler lands". `rp` manages cooling
through two MCP tools, `start_cooldown` and `start_warmup`, driven by a
per-camera **setpoint ladder**: the workflow decides *when* to cool and
when to warm, `rp` decides *which* rung.

### The setpoint ladder

`equipment.cameras[].cooler_targets_c` lists exactly the temperatures
the operator maintains dark libraries for — integers on a 5 °C grid
(−40 … +15 °C), no duplicates, order irrelevant:

```json
"cooler_targets_c": [-10, 5]
```

An empty (or absent) list means `rp` never touches that camera's cooler
(guide cameras, uncooled cameras). The governing invariant: **`rp` only
ever regulates at a listed temperature.** A single absolute setpoint
would waste winter cooling headroom (dark current roughly halves every
5–6 °C); a relative-to-ambient target would drift night to night and
never match a library.

Off-grid values, duplicates, and out-of-range values are rejected at
config load and `config.apply` with a field error naming
`equipment.cameras.N.cooler_targets_c`. The field's JSON Schema is an
`array` whose `items` enumerate the grid, so the web UI renders it as a
checkbox grid without hardcoding the rungs — see
[`ui-htmx.md`](ui-htmx.md) "Schema-driven rendering".

### Selection: `start_cooldown`

`start_cooldown` (ungated — a cooler setpoint is indoor work, § Safety →
[In-Flight Tool Calls](#in-flight-tool-calls)) spawns one background
cooldown task per camera with a non-empty ladder and returns at once
with the cameras it is driving (`{"cameras": ["main-cam"]}`; cameras
with an empty ladder are never listed, never touched). The workflow
carries on — slew, center, focus — while the sensor settles, so imaging
preparation is never blocked on thermal settling. Frames captured before
stabilization record their actual sensor temperature (see
[Per-frame recording](#per-frame-recording)), so they are identifiable
afterwards. The shipped workflow documents call it right after
`unpark`.

The tool is idempotent, so a document re-issues it freely — on every
start, on every resume after a crash or an `rp` restart:

- a cooldown pass already running for the camera is left to finish
  (re-issuing mid-pass neither restarts the pass nor re-selects);
- a cooler found **on and holding a configured rung** (`CoolerOn`
  true, `SetCCDTemperature` equal to a ladder entry, `CCDTemperature`
  within `cooling.tolerance_c` of it and, when readable, `CoolerPower`
  at or below `cooling.max_cooler_power_pct`) is adopted as-is — no
  command, no re-selection, no duplicate `cooler_stabilized`. The
  camera driver, not `rp`, is the source of truth for cooler state, and
  re-selecting mid-night would split the night across dark libraries. A
  rung merely *commanded* but not yet reached — an `rp` restart
  mid-pass — is not adopted: the pass below runs (re-commanding the
  same rung), so floor detection is never skipped;
- a warm-up ramp still running is cancelled and superseded by a fresh
  pass;
- anything else (cooler off, an off-grid setpoint, a failed read) runs
  the pass below.

A camera reporting `CanSetCCDTemperature == false` (or whose capability
read fails) is skipped with a `warn!` — a configured ladder on a
cooler-less camera is a config mismatch worth surfacing, not a fatal
error.

The task runs a **single cooldown pass**:

1. Command the **lowest** rung (`SetCCDTemperature`, then
   `CoolerOn = true`) and poll `CCDTemperature` — plus `CoolerPower`
   when `CanGetCoolerPower` — every `cooling.poll_interval`.
2. **Stabilized** — the temperature has stayed within
   `cooling.tolerance_c` of the commanded rung for a full
   `cooling.plateau_window` and cooler power is at or below
   `cooling.max_cooler_power_pct` (the power criterion is skipped when
   power is unreadable): the rung is adopted and `cooler_stabilized` is
   emitted.
3. **Floor detected** — the trajectory plateaus (total movement below
   `cooling.plateau_threshold_c` across a full `plateau_window`) while
   still warmer than rung + tolerance, *or* holds the rung only at
   power above the threshold (a rung held at 98 % power has no
   regulation authority left): the plateau temperature is tonight's
   floor. Snap **up** to the lowest rung at or above
   floor + `cooling.regulation_margin_c` and keep polling until
   stabilized there. Selection only ever moves up.
4. **No rung reachable** — even the warmest rung is below
   floor + margin: the cooler is switched **off** (never regulate
   off-grid), a `warn!` is logged, `cooler_unreachable` is emitted, and
   the night proceeds uncooled with every frame recording its actual
   temperature. Failing the tool instead is deliberately *not* the
   default — an unattended rig keeps imaging and the operator decides
   in the morning; an opt-in abort knob is a future consideration.
5. `cooling.max_cooldown` bounds the whole pass: on expiry the current
   temperature is treated as the floor and step 3/4 decides.

The chosen rung is **held until `start_warmup`** — re-selecting
mid-night would split one night's lights across dark libraries, and
selecting at dusk is conservative because ambient only falls until
dawn. Transient `CCDTemperature`/`CoolerPower` read failures during the
pass are retried like any idempotent Alpaca read and otherwise skip a
sample; they never abort the pass.

### Across an rp restart

`rp` never touches a cooler on its own: not at startup, not on a
safety transition
(no-actuation-on-connect tenet, [`workspace.md`](../workspace.md#project-tenets)
§ Project Tenets — and a regulated cold sensor is not a hazard, so a
supervisory transition has nothing to secure). The camera keeps its
last commanded setpoint across an `rp` restart — the driver keeps
regulating — while `rp`'s own record of the held rung starts empty, so
captures stamp `sensor_temperature_c` but no `cooler_setpoint_c` until a
workflow re-issues `start_cooldown`, which adopts the regulating rung as
described above. A workflow that resumes after an outage therefore gets
its rung back with one idempotent call and no thermal cycle.

The two tools are the controller's only entry points: no `rp` code
path — startup, a safety transition, a config reload — calls them.

### Warm-up: `start_warmup`

`start_warmup` (ungated — it secures, like `park`) starts a warm-up ramp
per camera `rp` is cooling and returns at once with those cameras
(`{"cameras": [...]}` — empty when `rp` commands none): the rung is
cleared immediately (frames captured during the ramp are off the grid),
`cooler_warmup_started` is emitted, the setpoint rises +5 °C every
`cooling.warmup_step_interval` until it reaches the warm target
(`HeatSinkTemperature` when the camera implements it, else
`cooling.warm_target_c`), then the cooler switches off and
`cooler_warmup_complete` is emitted. The ramp avoids thermal shock and
condensation/frost on the sensor window. Cameras `rp` never commanded
are untouched. Idempotent: a ramp already running is left to finish.

The shipped workflow documents call it in their `finally` blocks, so a
run that ends any way — complete, failed, or terminated for safety —
leaves the cooler warming; whether a safety termination warms up is
thereby the document's decision, not `rp`'s. `rp` itself leaves the
cooler alone on both safety transitions: it holds its rung through an
interruption. A `start_cooldown` issued during a ramp cancels it and
begins a fresh pass; symmetrically, a `start_warmup` that lands while a
cooldown pass is still commanding the device takes it over — the
commanded setpoint is recorded before the first mutating call, so the
cooler is never left regulating with nobody driving. `rp` shutting down
mid-ramp simply leaves the cooler at its last commanded setpoint — the
driver keeps regulating, and the next `start_cooldown` adopts or
re-selects.

### Per-frame recording

`capture` stamps two fields on every exposure document (see
[Exposure Document](#exposure-document)):

- `cooler_setpoint_c` — the rung currently commanded for the capturing
  camera; absent when `rp` is not cooling it (empty ladder, skipped,
  or uncooled after `cooler_unreachable`).
- `sensor_temperature_c` — a best-effort `CCDTemperature` read at
  capture time; absent when the read fails or the camera does not
  implement it. Read for every camera, ladder or not.

A night where selection failed is thereby identifiable frame by frame
instead of silently polluting stacks.

### Cooling events

Four point events (no `operation_id`) cover the lifecycle:
`cooler_stabilized`, `cooler_unreachable`, `cooler_warmup_started`,
`cooler_warmup_complete` — payloads in the [Events](#events) table.

### Tuning

The optional top-level `cooling` block tunes the controller; every
field has a default and the block is normally omitted:

| Field | Default | Meaning |
|-------|---------|---------|
| `poll_interval` | `"10s"` | Cadence of `CCDTemperature`/`CoolerPower` polling during cooldown |
| `plateau_window` | `"2m"` | How long a trajectory must persist to count as stable/plateaued |
| `plateau_threshold_c` | `0.5` | Movement below this across a full window = plateau |
| `tolerance_c` | `1.0` | "At the rung" means within this of the setpoint |
| `max_cooler_power_pct` | `90` | Stabilization requires power at or below this (regulation headroom) |
| `regulation_margin_c` | `3.0` | Chosen rung must sit at least this far above the measured floor |
| `max_cooldown` | `"20m"` | Hard bound on the whole selection pass |
| `warmup_step_interval` | `"2m"` | Time between +5 °C warm-up steps |
| `warm_target_c` | `10.0` | Warm-up endpoint when `HeatSinkTemperature` is unavailable |

Ambient-aware preflight (skipping obviously unreachable rungs using an
ObservingConditions device) and automated dark-library capture per rung
are future considerations — see
[Future Considerations](#future-considerations).

## Orchestration

`rp` does not contain workflow logic, and it does not start, supervise
or resume the process that does. The imaging workflow — what to do, in
what order, and when to switch targets — is driven by an
**orchestrator**: an MCP client that a person, a scheduler or the
orchestrator service's own startup starts, and that calls tools on `rp`
until it is done. `rp` registers nothing for it, keeps no record of it,
and treats its calls like any other client's (decision D6 in
[`mcp-sessionless.md`](../plans/mcp-sessionless.md)).

Different imaging types use different orchestrators:

| Workflow | Shape | Ships as |
|----------|-------|----------|
| deep-sky | slew → center → focus → capture loop, with refocus triggers, meridian flips, planner-driven target switching | `session-runner` document `deep_sky.json` (guide/dither steps join it as the remaining slice of the guider integration, issue #464) |
| calibrator-flats | read cover state → close cover → calibrator on → per-filter: find exposure time iteratively (halving panel brightness while pinned over-bright) → capture N flats → calibrator off → restore the cover's initial state | the Rust `calibrator-flats` service **and** its `session-runner` document port `calibrator_flats.json` (behavioral equivalence proven against the Rust suite; both are kept deliberately — D13 in `mcp-sessionless.md`) |
| sky-flat | point at the zenith → per-filter during twilight: capture with per-frame exposure adaptation against the changing sky | `session-runner` document `sky_flat.json` |
| planetary | slew → focus → high-fps capture, no guiding or plate solving | not yet built |

**How orchestrators are built.** An orchestrator can be a hand-written
service in any language (like `calibrator-flats`, Rust) **or** a
declarative **workflow document** executed by the generic
[`session-runner`](session-runner.md) service. `session-runner` is the
home of the first-party workflow documents: `deep_sky.json`,
`calibrator_flats.json`, and `sky_flat.json` ship in
`services/session-runner/workflows/` and install with that service —
one `session-runner` runs whichever document a `POST /runs` names.
`rp` cannot tell the difference between the two shapes — both are MCP
clients, and hand-written services remain first-class (decision D2 in
[`workflow-dsl.md`](../plans/archive/workflow-dsl.md)). Authoring
documents is covered in
[`docs/references/workflow-documents.md`](../references/workflow-documents.md);
the format and engine are specified in
[`session-runner.md`](session-runner.md).

### What `rp` Owns vs. What the Orchestrator Owns

**`rp` owns** (enforced regardless of which orchestrator runs):

- **MCP tool server** — all equipment, guider, compute, and planner
  tools.
- **Event bus** — emits events to webhook subscribers and the real-time
  stream.
- **Safety enforcement** — polls SafetyMonitors. On an unsafe
  transition, `rp` cancels every in-flight gated call, aborts
  exposures, stops guiding, parks the mount, and refuses gated tools
  until conditions are safe again. The orchestrator cannot prevent or
  delay this; it learns of it from the `cancelled: safety` tool error,
  the `SafetyUnsafe` refusal and the `safety_changed` event
  (see [Safety](#safety)).
- **Progress** — derived from the frames on disk, so no client has to
  persist a count and a restart resumes at the true count rather than
  at zero.

**The orchestrator owns** (implemented as client logic):

- **Workflow state machine** — the sequence of operations (slew, center,
  focus, guide, capture, dither, meridian flip, etc.).
- **Capture loop** — deciding when to start/stop exposures, managing
  multi-camera coordination, barrier synchronization.
- **Conditional logic** — when to refocus (temperature drift, HFR
  degradation), when to take flats, how to handle meridian flips.
- **Sub-workflow delegation** — the orchestrator can call compound tools
  provided by other plugins (e.g., `auto_focus`, `center_on_target`)
  or implement sub-workflows directly using primitive tools.
- **Its own lifecycle** — starting the run, persisting whatever it
  needs to resume, waiting out an unsafe spell or an `rp` outage, and
  resuming itself after its own restart. `session-runner` does all of
  this in-process ([session-runner.md § Runs](session-runner.md#runs),
  [§ Safety Behavior](session-runner.md#safety-behavior)).

### Orchestrator Lifecycle

```
rp starts
  → validates config, connects to equipment
  → builds MCP tool catalog (built-in + plugin-provided)
  → polls the safety monitors once, then starts the MCP server, event
    bus and safety polling
  → serves whoever connects

An operator (or a scheduler) starts a run at the orchestrator
  → the orchestrator connects to rp's MCP server
  → the orchestrator drives the run using tool calls
  → rp emits events as tools execute (exposure_started, slew_complete, etc.)

Safety event (unsafe transition)
  → rp cancels in-flight gated calls ("cancelled: safety"), aborts
    exposures, stops guiding, parks the mount, and closes the gate
  → the orchestrator pauses its run and waits (session-runner: state
    "paused", reason "safety"); its own persisted state survives
  → on the safe transition rp opens the gate and emits safety_changed;
    the orchestrator confirms safe conditions and resumes from its
    persisted state — rp re-invokes nobody

rp restarts mid-run (crash, power failure, systemd restart)
  → rp restores configuration and reconnects equipment; it has no run
    state to restore (§ What Survives an rp Restart)
  → the orchestrator's calls fail while rp is down; it pauses (reason
    "rp_outage"), reconnects with backoff, and resumes once rp answers

The orchestrator restarts mid-run
  → it resumes its own persisted run behind a reachable rp and a safe
    reading (session-runner's resume_on_start)

Run ends (workflow completes, operator stops it, or dawn)
  → the orchestrator disconnects from MCP; rp notices nothing
```

### Example: Deep-Sky Orchestrator Flow

The deep-sky orchestrator implements the classic imaging workflow. This
is what a typical orchestrator looks like — it's a program that calls
tools:

```
Orchestrator connects to rp MCP server

Loop:
  → tools/call get_next_target {}
  ← {name: "M31", ra: 10.6847, dec: 41.2689, filter: "Luminance", ...}

  → tools/call slew {ra: 10.6847, dec: 41.2689}
  ← {actual_ra: 10.6845, actual_dec: 41.2688}

  → tools/call center_on_target {ra: 10.6847, dec: 41.2689, tolerance: 5}
    (compound tool — centering plugin handles internally)
  ← {final_error_arcsec: 2.1, attempts: 3}

  → tools/call auto_focus {camera_id: "main-cam", focuser_id: "main-focuser"}
    (compound tool — focus plugin handles internally)
  ← {best_position: 12450, best_hfr: 2.1}

  → tools/call start_guiding {}
  ← {rms_ra: 0.4, rms_dec: 0.3}

  Capture loop:
    → tools/call capture {camera_id: "main-cam", duration: "5m"}
    ← {image_path: "...", document_id: "doc-042"}
    → tools/call record_exposure {target: "M31", filter: "Luminance"}
    ← {progress: [{filter: "Luminance", binning: "1x1", exposure_duration: "5m",
                   desired_count: 40, good: 13, total: 13}, ...]}
    → check if dither needed → tools/call dither {pixels: 5}
    → check if temperature drifted → tools/call auto_focus {...}
    → check if meridian flip needed → stop guide, flip, re-center, re-focus, start guide
    → tools/call get_next_target → if target changed, break capture loop

  → tools/call stop_guiding {}
  → continue outer loop with new target
```

### Compound Tools

Sub-workflows like `auto_focus` and `center_on_target` are **built-in
compound tools** — they live in `rp`'s process, drive a multi-step
loop using primitive built-in tools, and expose a single high-level
tool to the orchestrator. The orchestrator does not need to know the
focus algorithm or the centering algorithm; it calls one tool and
gets a result.

```
Orchestrator                    rp (single process)
    │                           ┌───────────────────────────────┐
    │  tools/call auto_focus    │                               │
    ├──────────────────────────►│  auto_focus impl (Rust)       │
    │                           │   ├─ move_focuser             │
    │                           │   ├─ capture                  │
    │                           │   ├─ measure_basic            │
    │                           │   │   (cache hit, no decode)  │
    │                           │   ├─ ... 12 more iterations   │
    │                           │   └─ pick best_position       │
    │  ← {best_position, hfr}  │                               │
    │◄──────────────────────────│                               │
    │                           └───────────────────────────────┘
```

No MCP-over-HTTP hop, no FITS re-decode (the in-process call resolves
each capture's pixels via the image cache).

#### `auto_focus` Contract

A built-in compound tool that drives a V-curve focus sweep using
`move_focuser`, `capture`, and `measure_basic` internally. The
orchestrator calls one tool and gets back the best focuser position
without having to know the focus algorithm.

**Input**:
- Addressing — exactly one form:
  - `camera_id` + `focuser_id` — the explicit pair, or
  - `train_id` — an `equipment.optical_trains[]` id, mutually
    exclusive with both explicit ids. Resolves `camera_id` to the
    train's terminal camera and `focuser_id` to its terminal focuser
    (error when the train has no focuser). Addressing the **guiding
    train** selects the PHD2-metric sweep instead of the capture
    sweep — guide-train AF never captures through the guide camera
    (PHD2 may own it at the SDK level); it moves the focuser and
    reads PHD2's per-frame HFD. See
    [Guide-train sweep](#guide-train-sweep-phd2-metric-variant).

  When addressed by `train_id`, every sweep parameter below
  additionally falls back, field by field, to the train's
  `auto_focus` config block before the "missing required parameter"
  check. Explicit-pair addressing takes per-call parameters only —
  the config block is train-scoped by design.
- `duration` — required, humantime string (same shape as `capture`'s
  `duration`, e.g. `"3s"`, `"500ms"`). Per-frame exposure for every
  point in the sweep. No default: the right value depends on focal
  ratio, sky brightness, and target field, none of which `auto_focus`
  can infer. Deriving it from a probe `measure_basic` star count was
  considered and rejected — the probe itself runs at unknown focus,
  so its star count is unreliable as a driver for the rest of the
  sweep.
- `step_size` — required, positive integer focuser steps. Required
  for the same reason `min_area`/`max_area` are required on
  `measure_basic`: focuser step → µm and rig depth-of-focus vary per
  setup and `rp` cannot infer them.
- `half_width` — required, positive integer. The sweep covers
  `[current_position − half_width, current_position + half_width]`
  in `step_size` increments. The grid is then clamped to the
  focuser's `min_position`/`max_position` from the `FocuserConfig`.
- `min_area` and `max_area` — required, passed through to each
  per-frame `measure_basic` call. At extreme defocus, donut-shaped
  PSFs from the secondary obstruction can span many hundreds of
  pixels — set `max_area` accordingly so the wings of the V-curve
  remain measurable.
- Optional `threshold_sigma` (default `5.0`) — passed through to
  `measure_basic`.
- Optional `min_fit_points` (default `5`) — minimum number of
  non-null HFR samples required to fit the V-curve. Also enforced
  on the *grid size* before any motion happens — a sweep that
  cannot produce at least this many capture positions errors before
  moving the focuser.

**Output**:
- `best_position` (i32) — focuser position at the fitted V-curve
  minimum, rounded to the nearest integer step.
- `best_hfr` (f64) — fitted HFR at `best_position`, in pixels.
- `final_position` (i32) — position the focuser was actually moved
  to at the end of the run. Equal to `best_position` on success.
- `samples_used` (usize) — number of HFR samples that contributed
  to the fit (i.e. captures with `star_count > 0` and a non-null
  HFR). `≤ curve_points.length`.
- `curve_points` — array of
  `{position: i32, hfr: f64 | null, star_count: u32, document_id: string}`,
  one entry per capture, in sweep order. `hfr: null` flags a
  starless capture: the entry is preserved as a record but does
  not contribute to the fit.
- `temperature_c` (f64 | null) — focuser temperature read once at
  the start of the run. `null` when the focuser does not implement
  temperature readout (`NOT_IMPLEMENTED`) **or** when the read
  itself fails for any other reason. Temperature is informational
  on the result and never load-bearing on the sweep, so a transient
  read failure does not abort the run — the field is just
  surrendered to `null`. Useful for downstream
  temperature-compensation logic that records
  `(position, temperature)` pairs across runs (callers that need
  absolute temperature confidence should fall back to
  `get_focuser_temperature` per call rather than relying on this
  field).

**Algorithm**:
1. Resolve `camera_id` and `focuser_id` (train addressing has
   already been reduced to the pair at this point). Read the focuser's current
   position and temperature once each; record both on the result.
   Emit `focus_started` carrying the resolved ids, the current
   position, and the temperature.
2. Compute the sweep grid:
   `start = current_position − half_width`; positions are
   `start, start + step_size, start + 2·step_size, …`, continuing
   while `≤ current_position + half_width`. Clamp the grid to
   `[min_position, max_position]` (any point outside is dropped,
   not coerced — coercion would create duplicate sweep positions
   at the bound). Reject before any motion if the clamped grid has
   fewer than `min_fit_points` positions.
3. For each grid position, in order:
   1. `move_focuser(position)` — block until the focuser reports
      idle (same poll loop the primitive `move_focuser` tool uses).
   2. `capture(camera_id, duration)` — yields `document_id`. The
      pixels populate the image cache as a side effect.
   3. `measure_basic(document_id, min_area, max_area, threshold_sigma)`
      — yields `hfr` and `star_count`. The cache hit avoids any
      FITS decode.
   4. Append `{position, hfr, star_count, document_id}` to
      `curve_points`. A capture with `star_count == 0` (or a
      `null` HFR for any reason) is recorded with `hfr: null` and
      contributes nothing to the fit.
4. If fewer than `min_fit_points` entries have a non-null HFR,
   abort with a `not_enough_stars` error. The focuser is left at
   the last sweep position; `auto_focus` does not auto-recover
   the original position.
5. Fit a parabola in raw HFR vs. position by least squares,
   weighted by `star_count` per point. From the fit
   `hfr = a·position² + b·position + c`:
   `best_position = round(−b / 2a)`; `best_hfr = c − b²/(4a)`.
   Abort with a `monotonic_curve` error in any of three cases:
   (i) the design matrix is singular at fit time (essentially
   flat HFR over the sweep — no parabola can be fitted), (ii)
   `a ≤ 0` (the curve is monotonic or concave-down — no minimum
   exists), or (iii) `a > 0` but the fitted vertex falls outside
   `[min(grid), max(grid)]` (a true minimum exists somewhere
   off-grid, so the visible curve is monotonic *over the sampled
   range* — the caller needs to widen the sweep or coarse-focus
   first).
6. Move the focuser to `best_position` (already inside the sweep
   range by construction, so the operator-supplied
   `min_position`/`max_position` bounds are guaranteed to hold).
7. Emit `focus_complete` with
   `{camera_id, focuser_id, position: best_position, hfr: best_hfr, samples_used}`.

**Error cases**:
- `train_id` passed together with `camera_id` or `focuser_id`, or
  no addressing at all → MCP error naming the conflict / the first
  missing field.
- `train_id` unknown → MCP error naming the train.
- The train has no focuser → MCP error naming the train and the
  reason.
- `camera_id` not found → MCP error naming the camera.
- `focuser_id` not found → MCP error naming the focuser.
- Camera or focuser not connected → MCP error.
- `duration`, `step_size`, `half_width`, `min_area`, `max_area`
  missing → MCP error naming the missing parameter (validated in
  body in input order, same convention as `measure_basic`).
- `step_size <= 0`, `half_width <= 0`, or `min_fit_points < 3`
  → MCP error naming the bad parameter (a parabolic fit needs at
  least 3 non-collinear points).
- Estimated unclamped grid size (`2·half_width / step_size + 1`)
  exceeds the safety cap (1000 points) → MCP error before any
  motion or exposure. The cap is purely a guardrail against
  operator misconfiguration that would otherwise produce
  thousands of captures and tie up the rig for hours; any
  plausible auto-focus run fits well inside it (typical 10–30
  points).
- Sweep grid (after clamping to `min_position`/`max_position`)
  has fewer than `min_fit_points` positions → MCP error before
  any motion or exposure. The error message names `min_fit_points`
  so the caller can tell the grid-size failure apart from a
  parameter-validation failure.
- A `move_focuser`, `capture`, or `measure_basic` call inside
  the sweep returns an error → `auto_focus` propagates that
  error and stops sweeping. Captures already taken are persisted
  on disk normally (that path is owned by `capture`, not
  `auto_focus`); the focuser is left at its current position.
- Fewer than `min_fit_points` non-null HFR samples after the
  sweep completes → `not_enough_stars` error.
- Parabolic fit yields no meaningful minimum within the sampled
  range → `monotonic_curve` error. This fires when the design
  matrix is singular (the input is essentially flat HFR), when
  `a ≤ 0` (the curve is monotonic or concave-down — no minimum
  exists), or when `a > 0` but the fitted vertex falls outside
  `[min(grid), max(grid)]` (a true minimum exists somewhere
  off-grid, so the *visible* curve over the sampled range is
  monotonic). The caller is expected to widen `half_width`,
  coarse-focus externally, or both, then retry. The focuser is
  **not** automatically moved to the lowest observed sample,
  because that point is unverified as a true minimum.

**Persistence**: `auto_focus` does **not** write a section on any
single exposure document — its result spans the sweep. Each capture
inside the sweep gets its own `image_analysis` section written by
the embedded `measure_basic` call exactly as if `measure_basic` had
been called directly. The compound result is returned in the MCP
response and emitted as `focus_complete`. Each entry in
`curve_points` carries the per-step `document_id`, so callers that
need per-step provenance can fetch the individual exposure documents
and read their `image_analysis` sections.

**Caveats**:
- Parabolic fit is the V1 choice for simplicity. Real V-curves are
  often slightly asymmetric (extra-focal vs. intra-focal slopes
  differ); a parabola fits an effective vertex that may sit one
  or two steps off the true minimum. Acceptable for amateur rigs.
  An asymmetric V or piecewise-linear fit can ship later as a
  built-in revision, or from a tool provider under its own name
  (e.g. `auto_focus_asymmetric_v`) — see
  [Third-party alternatives](#third-party-alternatives).
- No automatic re-sweep on a monotonic curve. The caller already
  knows what coarse-focus heuristic they prefer; `auto_focus`
  reports the failure cleanly and lets the caller widen
  `half_width` or coarse-focus externally before retrying.
  Adding re-sweep state-machine logic would double the BDD
  surface for marginal benefit.
- Saturated stars are included in `star_count` and contribute to
  the fit through their HFR, mirroring `measure_basic`'s policy.
  Filtering them at the auto-focus layer would reintroduce the
  HFR-vs-focus monotonicity break the policy was designed to
  avoid (see `measure_basic` Contract). Callers that need a
  per-curve saturation aggregate can fetch each
  `curve_points[i].document_id` and read its `image_analysis`
  section.
- `auto_focus` is a built-in compound tool; a tool-provider plugin
  advertising the same `auto_focus` name fails startup per
  Config-Time Validation. An alternative ships under its own name.

##### Guide-train sweep (PHD2-metric variant)

Addressing `auto_focus` with the guiding train's `train_id` runs the
same V-curve algorithm with a different sample source: instead of
`capture` + `measure_basic`, each grid position's sample is the
**median HFD of fresh PHD2 guide frames**, read from the guider
service's per-frame metrics window
(phd2-guider.md § `GET /api/v1/guiding/metrics`). The guide camera
is never captured through — PHD2 may own it at the SDK level.

Requirements, checked before any motion:

- `equipment.mount.guiding` is configured, and the guider's stats
  report an **active guide loop** — PHD2 only emits `GuideStep`
  (and with it HFD) while guiding, so without it there is nothing
  to measure. Error: "guide-train auto_focus requires active
  guiding". Guide **corrections stay active for the whole sweep**
  by the same logic; a defocusing star drifts little, and the
  alternative (paused output) stops the metric stream. (Whether
  HFD in fact streams in PHD2's paused modes is a rig-verification
  item; `get_star_image` polling is the recorded fallback should a
  real rig contradict this.)
- Sweep geometry comes from `step_size` + `half_width` — per-call
  or from the guiding train's `auto_focus` block, same field-by-field
  fallback as the capture sweep. The capture-only parameters
  (`duration`, `min_area`, `max_area`, `threshold_sigma`) are
  **rejected** when passed per-call with a guiding `train_id`, and
  rejected at config load inside a guiding train's block — a
  parameter that cannot influence the run must not pretend to.
- `frames_per_step` (config-block only, default `3`): a positive
  integer of fresh frames per grid position, at most 50 — the
  guider's metrics window; a larger value could never be satisfied
  and is rejected at load.

Per grid position: `move_focuser` (the guiding train's terminal
focuser), then refresh the freshness watermark from the metrics
window — frames exposed *during* the focuser motion, at a stale
focus, never count — and poll until `frames_per_step` frames above
it arrive (bounded by a fixed 30 s-per-frame ceiling — guide
exposures are seconds; expiry errors the run, and a metrics
response reporting guiding stopped fails it immediately). The **earliest**
`frames_per_step` fresh frames form the position's sample set, so a
slow poll cannot inflate it; the sample is the median HFD of the
valid ones among them. Frames flagged `star_lost` or with a null
HFD are **invalid**: a sample set with no valid frame records a
null sample — at deep defocus the star genuinely vanishes, so null
samples at the sweep edges are the expected bracket shape, exactly
like starless captures in the capture sweep.

Fit, recovery, and result mirror the capture sweep (`min_fit_points`
valid samples required; samples weighted by their valid-frame count —
the capture sweep's star-count weighting, one metric over;
`not_enough_stars` / `monotonic_curve` errors; the focuser moves to
the fitted minimum on success), with
two shape differences: the result reports `best_hfd` (PHD2's
half-flux **diameter**, in guide-camera pixels) instead of
`best_hfr`, and `curve_points` entries are
`{position, hfd: f64 | null, frames_used: u32}` — there are no
`document_id`s because nothing was captured. `focus_started` /
`focus_complete` / `focus_failed` payloads gain
`method: "capture" | "phd2_hfd"` on both sweep variants.

#### `refocus_train` Contract

A built-in compound tool that expands one refocus trigger on a train
into the dependency-ordered auto-focus sequence derived from the
train model (see [Optical Trains](#optical-trains)). One call, the
right AF runs in the right order — the caller does not need to know
which focusers are shared or which train each one focuses.

**Input**:
- `train_id` — required, an `equipment.optical_trains[]` id.
- `reason` — optional free-form string recorded on `refocus_started`
  and the result (e.g. `"temperature_drift"`, `"filter_change"`);
  defaults to `"manual"`. An orchestrator reacting to the
  [Guide Focus Watch](#guide-focus-watch) events passes reasons like
  `"guide_focus_degraded"`.

There are no per-call sweep parameters: steps span trains, and sweep
geometry is per-train — each step takes its parameters from its
**run train's** `auto_focus` config block, which must be present.

**Expansion**: the train model's AF sequence — shared focusers of
the train upstream-first, each run in the train where that focuser
is *terminal* (capturing through that train's camera), then the
train's own terminal focuser. Each step is one full V-curve run with
the semantics of the [`auto_focus` Contract](#auto_focus-contract),
including its per-step `focus_started` / `focus_complete` /
`focus_failed` triple. A step whose run train is the **guiding
train** is the
[PHD2-metric sweep](#guide-train-sweep-phd2-metric-variant) — it
requires an active guide loop, checked with the rest of the
expansion before any motion. Steps run strictly sequentially, and
the sequence derivation puts any guiding-train step last.

**Guiding handshake**: when `equipment.mount.guiding` is configured
and at least one **capture-based** step moves a focuser that is a
member of the guiding train, rp reads the guider's stats first; if
guiding is active, it pauses guide *corrections* (output-only — the
guide camera keeps looping) before the first step and resumes after
the last capture-based step. Sweeping a guide-coupled focuser
mid-correction defocuses the guide star under PHD2's feet; pausing
output while the loop keeps running lets PHD2 re-acquire cleanly on
resume. A guiding-train **metric** step runs *after* that resume,
under active corrections — the metric sweep needs the `GuideStep`
stream (see its contract), so an expansion never ends with
corrections paused before its own guide step. When the stats read
fails or reports not-guiding, capture-based steps run without the
handshake (`guiding_paused: false`) — a broken guider service must
not block a refocus (Tenet 2) — but a guiding-train metric step
still requires active guiding and errors without it. A failed
*pause* after stats reported active guiding is an error (the
service is alive and refusing); a failed *resume* after the
capture-based steps completed is also an error — a night with
guiding silently left paused must not look like success.

**Events**: a `refocus_started` / `refocus_complete` /
`refocus_failed` operation triple wraps the sequence (payloads in
the [Events](#events) table; no predictive deadline yet — the
per-step AF runs carry their own event triples).

**Output**: `train_id`, `reason`, `guiding_paused`, and `steps` —
one entry per completed AF run, in run order:
`{focuser_id, train_id, camera_id, best_position, best_hfr,
samples_used}` for capture-based steps; a guiding-train metric step
reports `best_hfd` instead of `best_hfr` and a `null` `camera_id`
(nothing was captured).

**Error cases**:
- `train_id` unknown → MCP error naming the train.
- The train has no focusers → MCP error (nothing to refocus).
- The expansion contains a guiding-train metric step but the guider
  is not configured, the stats read fails, or guiding is not active
  → MCP error ("guide-train step requires active guiding"),
  validated with the rest of the expansion before any motion.
- A step's run train has no `auto_focus` config block → MCP error
  naming the train and the missing block. Validated for every step
  before any motion.
- A step's AF run fails → the sequence stops, later steps do not
  run, guiding is resumed if it was paused, and the tool error names
  the failed step and its underlying error. Completed steps are not
  rolled back — their fitted minima are good positions.
- Pause / resume failures per the Guiding handshake above.

Like every built-in tool, `refocus_train` cannot be shadowed: a
tool-provider plugin advertising the same name fails startup.

#### `plate_solve` Contract

A built-in tool that proxies to the `plate-solver` rp-managed
service over HTTP. The wrapper hides ASTAP's subprocess details and
returns a parsed WCS solution. See
[`docs/services/plate-solver.md`](plate-solver.md) for the wrapper's
own contract.

**Input**:
- `document_id` *or* `image_path` — at least one required. Both
  fields are optional at the serde level so the tool can produce
  consistent error messages mentioning `image_path` when both are
  omitted (matching the imaging-tool convention). When both are
  supplied, **`document_id` takes precedence** (consistent with
  `measure_basic` et al. — see [Built-in Tools](#built-in-tools)).
- `pointing_hint` — optional nested object
  `{ ra_deg: f64, dec_deg: f64 }`. Decimal degrees on the wire for
  both fields (the `_deg` suffix is intentional — Alpaca returns
  `RightAscension` in **hours**, but the wrapper takes degrees).
  Both inner fields are required when the object is present; the
  nested-object shape makes the both-or-neither contract structural
  rather than runtime-validated.
- `use_mount_hints` — optional `bool`, default `false`. When
  `true`, rp reads the current mount position
  (`right_ascension()` × 15 → degrees, `declination()` pass-through)
  and forwards as the wrapper's `ra_hint` / `dec_hint`.
  - Mutually exclusive with `pointing_hint`. Both supplied ⇒ error
    `provide explicit pointing_hint or use_mount_hints, not both`.
  - Requires a configured and connected mount. Mount absent / not
    connected / Alpaca read failure ⇒ error to caller (the caller
    explicitly opted in, so failures are surfaced rather than
    silently dropping to blind solve).
- `fov_hint_deg` — optional `f64`. Forwarded verbatim to the
  wrapper's `fov_hint_deg`. v1 has no per-camera FOV stash on the
  exposure document — callers pass this per request. Tracked by
  [issue #153](https://github.com/rusty-photon/rusty-photon/issues/153).
- `search_radius_deg` — optional `f64`. Per-call value overrides
  `plate_solver.default_search_radius_deg` from rp config. Both
  absent ⇒ omit from wrapper request ⇒ ASTAP uses its own default.
  The override matters for loaded-from-disk images where the
  configured rig default may not match.
- `timeout` — optional humantime string (e.g. `"30s"`). Forwarded
  to the wrapper's `timeout` field. Omitted ⇒ wrapper applies its
  own `default_solve_timeout`. **Distinct from**
  `plate_solver.timeout` in rp config, which is the rp HTTP-client
  outer timeout (the connection-side backstop per Tenet 1).

When neither `pointing_hint` nor `use_mount_hints` is supplied, the
wrapper falls back to a blind solve.

**Output** (matches the wrapper's `SolveResponseBody` field-for-field):
- `ra_center` (f64) — image-center right ascension in decimal
  degrees.
- `dec_center` (f64) — image-center declination in decimal degrees.
- `pixel_scale_arcsec` (f64) — arcseconds per pixel from the
  parsed `.wcs` `|CDELT1|`.
- `rotation_deg` (f64) — field rotation from `.wcs` `CROTA2`.
- `solver` (String) — solver banner from the wrapper (e.g.
  `"astap-2026.05.03"`).
- `wcs_matrix` (object or null) — the full WCS linear mapping,
  passed through from the wrapper verbatim: `crpix1`/`crpix2` in
  FITS 1-based pixels, `cd1_1`/`cd1_2`/`cd2_1`/`cd2_2` in degrees
  per pixel. `null` when the wrapper's `.wcs` sidecar lacked a
  complete six-key set — the wrapper never synthesizes a matrix
  from CDELT/CROTA2 (that would fabricate the image parity the CD
  determinant's sign encodes).

**Persistence**:
- `document_id` mode: writes a `wcs` section to the exposure
  document via `ImageCache::put_section`. Section payload mirrors
  the output verbatim.
- `image_path` mode: after a successful solve, derives the sibling
  sidecar path (`<base>.fits` → `<base>.json`) and resolves it to
  an `ExposureDocument` via `ImageCache::resolve_document_by_path`.
  If the sidecar exists and parses (the **late-solve workflow**:
  capture frame N → start capture N+1 → solve frame N → update the
  original sidecar), `wcs` is written via `put_section`. If no
  sidecar is present (external FITS, missing sidecar, non-`.fits`
  path), the result is returned without persistence and the cache
  miss is `debug!()`-logged. `put_section` itself falls back to a
  disk-only write when the cache entry is absent (post-eviction or
  post-`rp` restart) so the sidecar always sees the section update.
- Persistence failure (sidecar write error) is logged at `debug!()`
  and does *not* fail the tool — same shape as the imaging-tool
  convention.

**Error policy**:
- Configuration errors before the call:
  - rp `plate_solver` config absent ⇒
    `plate_solve: plate solver not configured`.
  - Hint mutual-exclusion violated ⇒
    `plate_solve: provide explicit pointing_hint or use_mount_hints,
    not both`.
  - `use_mount_hints: true` with mount issue ⇒
    `plate_solve: use_mount_hints requested but mount is <reason>`.
  - Neither `document_id` nor `image_path` ⇒ MCP error mentioning
    `image_path`.
- HTTP-client failures (DNS, refused, connection timeout) ⇒
  `plate_solve: service unreachable: <reason>`.
- Wrapper structured errors (`invalid_request`, `fits_not_found`,
  `solve_failed`, `solve_timeout`, `internal`) propagate verbatim
  as `plate_solve: <code>: <message>`. For `solve_failed`, the
  wrapper's `details.stderr_tail` is appended for diagnostics. rp
  does **not** pre-validate `fits_path` — the wrapper is
  authoritative.

**Algorithm**:
1. Validate `document_id` xor `image_path` (at least one). Validate
   hint shape (mutual exclusion + `use_mount_hints` requires
   connected mount).
2. Resolve `PlateSolveClient` from `AppState`. Error if absent.
3. Resolve `fits_path`:
   - `document_id` mode: `ImageCache::resolve_document(doc_id)` →
     `doc.file_path`.
   - `image_path` mode: forward verbatim.
4. Resolve hints into the wrapper's flat `ra_hint` / `dec_hint`
   pair. Explicit `pointing_hint` maps directly; `use_mount_hints`
   reads the mount and applies the ×15 RA conversion.
5. Resolve `search_radius_deg`: per-call value > config default >
   absent.
6. Build `SolveRequest`, call `client.solve(req)`. Map `SolveError`
   variants to MCP errors per the policy above.
7. On success: in `document_id` mode, persist the `wcs` section to
   that document. In `image_path` mode, attempt UUID-8 reverse-
   lookup; persist if matched, debug-log if not.
8. Return the solver output as the MCP tool result.

**Compound caller note**: `center_on_target` (planned, Phase 6c-3)
sets `use_mount_hints: true` on its inner `plate_solve` calls
rather than calling a Rust-side mount-read helper. That keeps the
hours→degrees conversion in one place (this contract) and avoids a
parallel code path for the same data flow.

`plate_solve` is a built-in tool; a tool-provider plugin advertising
the same `plate_solve` name fails startup per Config-Time Validation.
An alternative solver ships under its own name.

#### `center_on_target` Contract

A built-in compound tool that drives an iterative
capture → plate_solve → sync_mount → slew loop until the solved
field-center sits within `tolerance_arcsec` of the requested
`(ra, dec)`. The orchestrator calls one tool with the target
coordinates and gets back the converged pointing without having to
implement its own centering loop.

**Input**:
- `camera_id` — required. Camera that captures each iteration's
  frame.
- `ra` — required, decimal hours `∈ [0, 24)`. Target right
  ascension. Same unit as `slew` and `sync_mount` (Alpaca's
  `RightAscension`).
- `dec` — required, decimal degrees `∈ [-90, 90]`. Target
  declination.
- `duration` — required, humantime string (e.g. `"5s"`,
  `"500ms"`). Per-iteration exposure. No default: the right value
  depends on focal ratio, sky brightness, and target field, none
  of which `center_on_target` can infer. v1 uses the same
  `duration` for every iteration; if low star count blocks a
  solve, the caller re-runs with a longer duration.
- `tolerance_arcsec` — required, positive `f64`. Convergence
  threshold on the great-circle residual between the solved center
  and `(ra, dec)`. No default: the right value depends on rig
  pixel scale (a 1-arcsec/pixel rig wants tighter tolerance than a
  4-arcsec/pixel one) and downstream framing constraints.
- `max_attempts` — required, positive `usize`. Hard cap on the
  number of iterations. No default: the right value depends on
  mount tracking quality and how aggressive the caller wants the
  loop to be (typical 3–5 attempts; tight tolerances or wobbly
  mounts may want 10+). Capped at `MAX_ATTEMPTS = 50` — exceeding
  the cap is a parameter error before any motion. The cap is a
  guardrail against operator misconfiguration that would otherwise
  tie up the rig for an indefinite period; any plausible run fits
  well inside it.

The mount is resolved via the singular `mount` config field — no
`mount_id` or `telescope_id` parameter, since `rp` deployments run
exactly one mount.

**Output**:
- `final_error_arcsec` (f64) — great-circle residual at the
  iteration where convergence fired (i.e. the iteration whose
  `action` is `"converged"`).
- `attempts` (usize) — number of iterations executed. `≥ 1` and
  `≤ max_attempts` on success.
- `final_ra` (f64) — solved RA at the converged iteration, in
  decimal degrees (matches `plate_solve`'s output unit, **not**
  the input's hours).
- `final_dec` (f64) — solved Dec at the converged iteration, in
  decimal degrees.
- `iterations` — array of
  `{document_id: string, residual_arcsec: f64, solved_ra: f64,
  solved_dec: f64, action: "sync" | "slew" | "converged"}`,
  one entry per iteration in execution order. Each
  `document_id` carries the per-iteration capture's `wcs` section
  (written by the embedded `plate_solve`); the
  [`/api/documents/{id}`](#document-resolution) endpoint gives
  callers per-step provenance. `action` is the *terminal* action
  for that iteration: `"sync"` only ever appears on iter 1 (and
  only if iter 1 also slewed — i.e. `iterations[0].action ==
  "sync"` means iter 1 fired sync followed by slew); `"converged"`
  appears at most once and is always the last entry; every other
  entry is `"slew"`. The iter-1-converged-after-sync case
  collapses to `action: "converged"` on the single record (sync
  fired, residual was already inside tolerance, no slew issued).

**Algorithm**:
1. Resolve `camera_id` and the singular mount. Emit
   `centering_started` carrying `{camera_id, ra, dec,
   tolerance_arcsec, max_attempts}` plus an **advisory outer-loop
   deadline** (§2.5): `per_iter = duration +
   centering.solve_time_estimate + centering.slew_overhead_estimate`,
   `predicted_duration_ms = per_iter`, `max_duration_ms = max_attempts
   × per_iter`. rp does not enforce this (each inner `capture`/`slew`
   carries its own deadline, and the watchdog tracks only this outer
   loop); the two estimates come from the `centering` config block
   (defaults 30 s / 10 s).
2. For `iter = 0..max_attempts`:
   1. `capture(camera_id, duration)` → `document_id`. Cache
      populated as a side effect.
   2. `plate_solve(document_id)` with `use_mount_hints: true` so
      the wrapper gets the mount's currently-reported pointing as
      an `ra_hint`/`dec_hint` pair (hours→degrees conversion lives
      in the `plate_solve` handler — see
      [`plate_solve` Contract](#plate_solve-contract)). Yields
      `(solved_ra_deg, solved_dec_deg)` and writes a `wcs` section
      to the document as a side effect.
   3. If `iter == 0`: `sync_mount(solved_ra_deg, solved_dec_deg)`.
      The first solve is the absolute pointing reference;
      subsequent iterations rely on the mount honouring relative
      slews instead of re-syncing on every pass. Repeated syncs
      interact badly with model-building drivers (each sync gets
      treated as a new pointing-model entry, polluting the model)
      and are unnecessary once the absolute position is
      established. Sync fires *unconditionally* on iter 1, even if
      the residual is already inside tolerance — the mount's
      pointing model is calibrated for any caller that follows
      centering with further targeted slews.
   4. Compute `residual_arcsec = haversine(solved_ra_deg,
      solved_dec_deg, ra·15.0, dec)` (the input `ra` is hours; the
      solved values are degrees, so convert the input to degrees
      once for the comparison).
   5. If `residual_arcsec ≤ tolerance_arcsec`: record an
      `iterations[iter]` entry with `action = "converged"`, emit
      `centering_iteration` followed by `centering_complete`, and
      return.
   6. Otherwise: `slew(ra, dec)` honouring the mount's
      `settle_after_slew` config; record an `iterations[iter]`
      entry with `action = "slew"` (or `action = "sync"` on
      iter 1 if the slew was preceded by a sync — see Output's
      collapse rule); emit `centering_iteration` and continue.
3. If the loop exits without firing `"converged"`, return a
   `tolerance_not_reached` error carrying the last residual and
   `max_attempts`. The mount is left at its last commanded
   position; `center_on_target` does not auto-recover.

**Error cases**:
- `camera_id` missing or names a camera that doesn't exist /
  isn't connected → MCP error naming the field/condition (parameter
  validation runs in input order, same convention as
  `auto_focus` / `measure_basic`).
- `ra`, `dec`, `duration`, `tolerance_arcsec`, `max_attempts`
  missing → MCP error naming the missing parameter.
- `ra` outside `[0, 24)` or `dec` outside `[-90, 90]` → MCP error
  naming the bad parameter.
- `tolerance_arcsec ≤ 0` or `max_attempts == 0` → MCP error
  naming the bad parameter.
- `max_attempts > MAX_ATTEMPTS` (50) → MCP error before any
  motion or exposure.
- No mount configured / mount not connected → MCP error.
- Mid-loop `capture`, `plate_solve`, `sync_mount`, or `slew`
  failure → propagates the underlying error and aborts the loop.
  The mount is left where the failed step left it; partial
  `iterations[]` entries are not returned (the failure surfaces as
  an MCP error, not a partial success).
- Unresponsive device → bounded, never an indefinite hang. Every
  Alpaca request rp issues carries a per-request connect + read
  timeout (`equipment::alpaca`), so a device that accepts the
  connection but stops answering — an overloaded simulator in CI, a
  stalled mount/USB-serial bridge at night — surfaces as a timeout
  error instead of wedging the loop forever. The idempotent
  per-iteration mount reads (`plate_solve`'s `use_mount_hints` read;
  the slew's `Slewing` poll) additionally **retry** a transient
  failure with short backoff before giving up, so a brief device
  hiccup is ridden out rather than aborting the whole tool. (This is
  the fix for the issue #319 `center_on_target` timeout: a stalled
  mount read had no client-side timeout and hung indefinitely; the
  blocking-op poll deadlines guard loops, not a single in-flight
  request.)
- Progress emission during long polls → companion fix on the same
  PR (#319), kept after the transport went session-less
  (§ [MCP Server](#mcp-server)). While `rp` still ran rmcp's legacy
  sessions, their 300 s idle keep-alive raced a long slew, park or
  exposure: when both fired near the same moment the SSE response
  stream EOFed and the client's `call_tool` future never resolved
  (BDD's 360 s `MCP_CALL_TIMEOUT` was the only backstop). Each poll
  loop emits `notifications/progress` every `PROGRESS_INTERVAL` (5 s)
  — see `mcp::progress` — which reset that timer. There is no
  keep-alive left to reset, but the emission stays: a
  `progressToken`-bearing caller sees the loop advance, and it is a
  no-op when the client did not supply one in `_meta`; unit tests
  pass `None`.
- Loop exits without convergence → `tolerance_not_reached` error
  citing the last residual and `max_attempts`.

**Persistence**: `center_on_target` does **not** write a section
on any single exposure document. Each per-iteration capture gets
its own `wcs` section written by the embedded `plate_solve` call
exactly as if `plate_solve` had been called directly. The compound
result is returned in the MCP response and emitted as
`centering_complete`. Each entry in `iterations` carries the
per-step `document_id`, so callers that need per-step provenance
can fetch the individual exposure documents and read their `wcs`
sections.

**Caveats**:
- OmniSim does not model pointing — it returns canned exposures
  unaffected by mount position. Convergence over real pixels is
  asserted via the stub plate solver in BDD; the live OmniSim
  camera and mount still exercise the capture / sync / slew
  surface.
- v1 has no `min_improvement_arcsec` early exit. A run that stops
  improving but keeps barely missing tolerance will burn through
  `max_attempts` before erroring. Add the parameter when the first
  workflow needs it.
- v1 has no per-iteration exposure scaling. If the first solve
  fails for star-count reasons, the caller re-runs with a longer
  `duration` rather than letting the tool widen automatically.
- `center_on_target` is a built-in compound tool; a tool-provider
  plugin advertising the same `center_on_target` name fails startup
  per [Config-Time Validation](#config-time-validation). An
  alternative ships under its own name.

#### Example: `auto_focus` (V-curve)

See [`auto_focus` Contract](#auto_focus-contract) for the full parameter
set, error policy, and persistence rules. The pseudo-code below
illustrates the loop shape only.

```
Orchestrator: tools/call auto_focus {
    camera_id: "main-cam",  focuser_id: "main-focuser",
    duration: "2s",         step_size: 200,          half_width: 1500,
    min_area: 8,            max_area: 400
}
  rp's auto_focus implementation (current_position = 11000):
    move_focuser(position=9500) → 9500
    capture(camera_id="main-cam", duration="2s") → {document_id: "doc-001"}
    measure_basic(document_id="doc-001", min_area=8, max_area=400)
                                                   → {hfr: 6.8, star_count: 220}
    move_focuser(position=9700) → 9700
    ... 14 more sweep points (15 total) ...
    move_focuser(position=12500) → 12500   # last sweep point
    fit parabola → best_position = 11212
    move_focuser(position=11212) → 11212   # final move to fitted vertex
  ← {best_position: 11212, best_hfr: 2.1, final_position: 11212,
     samples_used: 15, curve_points: [...], temperature_c: 4.3}
```

#### Example: `center_on_target`

See [`center_on_target` Contract](#center_on_target-contract) for the
full parameter set, error policy, and persistence rules. The
pseudo-code below illustrates the loop shape only.

```
Orchestrator: tools/call center_on_target {
    camera_id: "main-cam",  ra: 10.6847,           dec: 41.2689,
    duration: "5s",         tolerance_arcsec: 5,   max_attempts: 5
}
  rp's center_on_target implementation:
    iter 0:
      capture(camera_id="main-cam", duration="5s")   → {document_id: "doc-c01"}
      plate_solve(document_id="doc-c01", use_mount_hints: true)
                                                     → {ra_center: 160.230°, dec_center: 41.265°}
      sync_mount(ra=10.6820, dec=41.265)            # 160.230° / 15
      residual_arcsec = haversine(...)               → 45.0    (> tolerance)
      slew(ra=10.6847, dec=41.2689)                  # iter-1 action: "sync"
    iter 1:
      capture → {document_id: "doc-c02"}
      plate_solve(document_id="doc-c02", use_mount_hints: true)
                                                     → {ra_center: 160.270°, dec_center: 41.2685°}
      residual_arcsec                                → 2.1     (≤ tolerance)
                                                     # action: "converged"
  ← {final_error_arcsec: 2.1, attempts: 2,
     final_ra: 160.270, final_dec: 41.2685,
     iterations: [
       {document_id: "doc-c01", residual_arcsec: 45.0, solved_ra: 160.230,
        solved_dec: 41.265,  action: "sync"},
       {document_id: "doc-c02", residual_arcsec: 2.1,  solved_ra: 160.270,
        solved_dec: 41.2685, action: "converged"},
     ]}
```

#### Third-party alternatives

A site that wants a different algorithm (parabolic-fit focus, ML-based
focus, plate-solve-driven centering with custom heuristics) ships it
from a tool provider under a *different* tool name (e.g.
`auto_focus_parabolic`). The orchestrator opts in by calling the
plugin's tool name — a `session-runner` document names the tool it
wants — and both algorithms stay reachable. There is no drop-in
replacement: a plugin advertising a built-in's name, or a name another
plugin already offers, fails startup per [Config-Time
Validation](#config-time-validation), because a catalog whose entries
depend on registration order is a fault that surfaces at 2 a.m.
(tenet 2). Which implementation a run uses is written in the document
that runs it, not inferred from which plugin happened to be configured.

## Planning and Ephemeris

`rp` ships the planner with an in-process astronomical math layer
(`rp-ephemeris`) and an embedded deep-sky object catalog
(`rp-catalog`). Together they let an orchestrator answer "image M41
right now" from a name string, without the operator having to paste
coordinates by hand or trust the mount's pointing model.

The implementation plan that introduces both crates lives at
[`docs/plans/archive/rp-planning-tools.md`](../plans/archive/rp-planning-tools.md).
This section describes the resulting design as it appears to MCP
clients and to operators editing the config file.

### Site Configuration

Site location is a top-level `site` block:

```json
"site": {
  "latitude_degrees": 47.6062,
  "longitude_degrees": -122.3321
}
```

Range validation: `latitude_degrees ∈ [-90, 90]`,
`longitude_degrees ∈ [-180, 180]`. Out-of-range values fail config
load with a named field.

The IANA timezone (`America/Los_Angeles`, `Europe/Madrid`,
`America/Santiago`, …) is derived once at startup from `(lat, lon)`
via the `tzf-rs` crate. System tzdata then supplies DST rules.
Startup logs an `info!("site: {site}")` line carrying lat/lon and
the derived timezone, so a misconfigured location surfaces as a
visibly-wrong timezone before it produces wrong twilight times.

`tzf-rs`'s default polygon dataset costs ≈128 MiB of resident memory
and bundles ODbL-licensed data; that is acceptable in this service's
deployment posture. The crate itself is MIT-licensed (an additional
"Anti CSDN License" footnote forbids the Chinese aggregator CSDN
specifically and has no practical effect on this workspace's use).

**No elevation in v1.** Sidereal time depends only on longitude, the
mount's refraction model handles pressure/temperature, and elevation
matters only for solar-system parallax (≤1° for the Moon, sub-arcsec
for planets) and horizon-dip in twilight (≈1° at 4000 m). Adding
`elevation_meters` later is a backwards-compatible config addition.

**No horizon profile in v1.** A single `min_altitude_degrees` (per
target, with a planner-wide default in the `planner` block) covers
the common case. Per-azimuth obstruction profiles are deferred.

### Site Validation Against the ASCOM Mount

`SiteLatitude` and `SiteLongitude` are standard ASCOM Telescope
properties. On telescope connect, `rp` reads both and compares to
config:

- If both reads succeed, any difference greater than `0.01°` (≈1 km)
  in either dimension is a **hard error** on connect, naming both
  pairs in the error message. Silently running ephemeris math
  against a site that disagrees with what the mount computes
  hour-angle from is the precise class of bug that produces
  plausible-looking wrong slew targets — i.e., the worst kind.
- If either read fails (typically `NOT_IMPLEMENTED` from a mount
  that does not expose the property — ASCOM has no
  `CanGetSiteLatitude` / `CanGetSiteLongitude` capability bit;
  the read attempt itself is the capability probe), config is the
  source of truth and a `debug!()` log notes that mount validation
  was skipped.

The mismatch threshold is not configurable: 0.01° is below the
positional accuracy any operator would set deliberately, and well
above any rounding noise on either side.

### The `Ephemeris` Trait

The math layer lives in the `rp-ephemeris` crate. Its design — the
`Ephemeris` trait surface, the `ErfarsEphemeris` ERFA wrapping,
the in-process posture, panic safety and NaN-degradation
guarantees, derived helpers, and time-scale treatment — is
documented in [`docs/crates/rp-ephemeris.md`](../crates/rp-ephemeris.md).

For `rp`'s purposes, the relevant guarantees are:

- All trait methods are pure functions returning owned values; no
  `unsafe`, no FFI types in the surface.
- ERFA failures (host-clock misconfiguration, upstream wrapper
  inconsistency) degrade to NaN coords / `None` windows with an
  `error`-level log — they never crash the rp orchestrator.
- ΔUT1 is treated as zero (≤ 0.9 s = ≤ 13″ of LST error) — well
  inside what plate-solving refines on a real frame.

### Catalog (`rp-catalog`)

`rp-catalog` ships one embedded catalog spanning two entry classes
(issue #767): ~19k deep-sky objects — Messier + NGC + IC (OpenNGC)
plus the astrophoto catalogs (Sharpless, Barnard, LDN, LBN, vdB, RCW,
Gum, Cederblad, Abell planetaries, Arp, Hickson, Collinder, Melotte,
Stock, Trumpler) — and ~354k stars, every HD/HDE/HDEC designation in
the Tycho-2/HD cross-index with J2000 Tycho-2-derived positions. The
~400 stars with IAU proper names are canonical under that name
(`"Vega"`); the HD designation resolves to the same entry. Per-source
attribution lives in `crates/rp-catalog/src/data/LICENSE-DATA`.

Resolution is case-, whitespace-, and dash-insensitive with
catalog-prefix rewrites — `"M41"`, `"Messier 41"`, `"NGC 2287"` (an
alias), `"Sharpless 101"` / `"Sh2-101"`, `"Barnard 33"`, and
`"HDE 227018"` / `"HD 227018"` all resolve. Object type and
approximate magnitude are returned alongside RA/Dec.

Everything is packed into a single committed binary
(`catalog.bin`, ~5.5 MB, generated by `scripts/pack_catalog.py` from
the committed per-catalog CSVs plus the fetched star layer) that the
crate reads zero-copy — lookups binary-search the blob in place, star
names are formatted on demand, and unit tests lock the blob to its
CSV sources so they cannot drift.

The catalog is offline-first: typing a Messier number and getting
coordinates is too core to require a plugin install, and offline
operation matters at remote dark sites. A future SIMBAD-backed plugin
ships its lookup as a tool-provider tool under its own name (say
`resolve_target_simbad`); a built-in's name cannot be taken over (see
[Config-Time Validation](#config-time-validation)).

`add_target` accepts either a `catalog_ref` name or literal RA/Dec —
catalog lookup is a tool call, not a config-time resolution.

### Primitive vs. Convenience MCP Tools

Both layers call the same internal `Ephemeris` trait. The split is
purely how operations are projected onto the MCP catalog: primitives
are one operation each (a planner plugin composes them); convenience
tools are the high-level shapes the default planner uses.

**Primitive tools** (one operation each):

| Tool | Returns |
|------|---------|
| `resolve_target {name}` | ICRS RA/Dec, object type, magnitude (catalog) |
| `compute_alt_az {ra, dec, time?}` | altitude, azimuth |
| `compute_transit {ra, dec, date}` | UT of upper transit |
| `compute_rise_set {ra, dec, date, min_alt_degrees}` | rise/set times |
| `compute_meridian_flip {ra, dec, time, side_of_pier}` | time-to-flip |
| `get_sun_position {time?}` | RA/Dec, alt/az |
| `get_twilight {date, kind}` | civil/nautical/astronomical begin/end |
| `get_moon_position {time?}` | RA/Dec, alt/az, phase, illumination |
| `compute_moon_separation {ra, dec, time?}` | angular separation |
| `get_local_sidereal_time {time?}` | LST |

**Convenience tools** (compose primitives, listed in
[Planner Tools](#planner-tools)): `get_target_status`,
`get_next_target`, `get_meridian_status`. `record_exposure` and
`get_session_progress` are orthogonal to ephemeris and not built on
this layer.

The chattiness cost of primitives is zero: planning runs at
target-switch cadence (minutes/hours), not per-frame. A plugin that
makes 20 MCP calls to compute "best target for the next 90 minutes"
is imperceptible.

## Target Store

*(P1 of [planetarium-target-import.md](../plans/planetarium-target-import.md)
— the store, its CRUD/goals MCP tools, and the planner's cutover to
reading the store have landed; the frame-scan-based progress derivation
below has not.)* The plan data model,
`TargetStore` trait, and `RedbTargetStore` implementation live in the
`rp-targets` crate ([design doc](../crates/rp-targets.md)); this
section is the authoritative rp-side integration contract that crate
doc's "rp Integration" section summarizes.

Targets are rows in a redb-backed `rp-targets` database that `rp` opens
once at startup (`target_store.db_path`, default
`<session.data_directory>/targets.redb`), editable live via the MCP
tools below without a restart — no more `PUT /api/config` plus a restart
to add or edit a target.

Targets live exclusively in the store: `get_next_target` (§ Dynamic
Planner), `record_exposure`, `get_session_progress`, and
`get_target_status` all read the store's active rows and nothing else.
`get_next_target` applies altitude gating to store rows (Decision 9,
below), reading `target.scheduling.min_altitude_degrees`, falling back
to `target_store.default_scheduling.min_altitude_degrees`, falling back
in turn to the planner-wide `planner.min_altitude_degrees`.

Progress everywhere — `get_target` / `list_targets`, the two progress
tools, and the planner's own decision logic — is **derived on demand
from the frames on disk** (§ Progress derivation). Nothing is stored:
`record_exposure` no longer increments a counter, because `capture`
already wrote the frame the derivation finds on its next read. One
consequence is the point of the whole target store: a target's plan
spans however many nights it takes to reach its goals, so frames
captured on previous nights count, and a resumed or restarted session
picks up where the disk left off rather than starting the night at
zero.

**Fixed migration requirements.** Two behaviors the planner already had
carried over to the store:

- **Altitude-gating parity (Decision 9) — landed.** The planner
  eliminates targets below `min_altitude_degrees` (§ Decision Logic,
  bullet 1); `get_next_target` applies that check to store-backed
  candidates — reading `target.scheduling.min_altitude_degrees`,
  falling back to `target_store.default_scheduling.min_altitude_degrees`
  from config, falling back in turn to the planner-wide
  `planner.min_altitude_degrees`. `add_target` / `update_target` accept a `scheduling`
  parameter (field-for-field `SchedulingConstraints`) so a caller can
  set the per-target override; `update_target`'s `scheduling`
  replaces the whole overrides object rather than merging field-wise.
  The other `SchedulingConstraints` fields (moon separation, moon
  illumination, meridian window) are stored but their *enforcement*
  stays deferred, per `rp-targets.md`'s MVP scope — this amends that
  crate doc's deferred list rather than silently overriding it,
  because altitude gating is not new ephemeris work (the shipped
  planner already evaluates it via `rp-ephemeris`).
- **A minimal operator surface (Decision 10) — landed.** The 6
  CRUD/goals MCP tools (`add_target` / `get_target` / `list_targets` /
  `update_target` / `set_goals` / `delete_target`) give list/edit/
  activate against the store, so a target that arrives with no UI (e.g.
  a planetarium-bridge import) is never stranded — this works with no
  UI running. Reachable only through MCP, gated the same as every
  other tool (§ Safety); the browser-facing target UI —
  [ui-htmx's targets inbox](ui-htmx.md#targets-inbox-targets) — is
  accordingly an MCP client like the orchestrator, not a REST caller.

### Slug allocation (add-time)

`add_target` derives and resolves the target's `TargetSlug` before
calling `TargetStore::upsert_target` (full algorithm and rationale in
[`rp-targets.md` § Slug allocation](../crates/rp-targets.md#slug-allocation-add-time)):

1. Base = `TargetSlug::new(catalog_ref.unwrap_or(display_name))`.
2. Absent in the store → use the base.
3. Present and the same object (matching `catalog_ref`, or coordinates
   within a small tolerance) → in-place edit: reuse the slug and
   upsert.
4. Present and a different object → allocate the lowest unused
   `"{base}-{n}"` (`n` from 2).

The same-`catalog_ref` branch is additionally gated on coordinate
proximity: a manual catalog add of an object whose framed coordinates
differ beyond tolerance from an existing same-`catalog_ref` row
allocates a new suffixed slug instead of overwriting it in place. This
protects a precisely-framed target (e.g. one that arrived via the P3
bridge) from being silently clobbered by a later catalog-centroid add
of the same object — the protection applies to every writer, not only
the bridge.

Every base — `catalog_ref` or `display_name`, add form or import form —
goes through the one derivation, `TargetSlug::from_display_name`:
whitespace runs collapse to a hyphen and the result is lower-cased, so
`"Comet Test"` → `comet-test` and `"NGC 7000"` → `ngc-7000`. Tools that
take a `slug` parameter parse it with `TargetSlug::new`, which rejects
whitespace rather than normalizing it and names the derived form in the
error. See [rp-targets.md § Identity](../crates/rp-targets.md#identity-the-slug)
for why the lossy step is deliberately confined to one function.

### Writer identity

`Target` carries `created_by` / `updated_by` writer-identity fields
beside the timestamps (settled in the P3 design, refining plan
Decision 3's notes-only provenance): every operator-surface write
(`add_target` without `source`, `update_target`, `set_goals`) stamps
`"operator"` on `updated_by`; an [import](#import-form-source) stamps
`source.kind`. The store preserves `created_at` *and* `created_by`
across `upsert_target` of an existing slug, so creation attribution
survives later edits. Rows written before these fields existed
deserialize as operator-owned (serde defaults; no redb schema step).
"Pending and unedited since import" is the first-class predicate
`!active && updated_by == source.kind` — what the import dedup below
keys on, and what the [ui-htmx targets
inbox](ui-htmx.md#targets-inbox-targets) reads as "who touched this
last".

### Import form (`source`)

`add_target`'s third parameter form, activated for the P3
planetarium bridge ([planetarium-bridge.md](planetarium-bridge.md)
§ rp-side contract is the design record; this section is the landed
contract): bare `ra_hours` + `dec_degrees` + `source {kind, client,
received_at}`. `catalog_ref`, `display_name`, `active`, `notes`, and
`position_angle_degrees` are all rejected alongside `source` — naming
is rp's job here, imports always land paused (`active: false`) with no
framing angle (no planetarium channel carries one; per-target angles
are entered in the ui-htmx targets inbox —
[Position angle](#position-angle)), and
rp writes the human-readable provenance line (`"Imported via <kind>
from <client> at <received_at>"`) into `notes` itself (display data,
never parsed).
`source.kind` must not be `"operator"` (reserved). `goals[]` defaults
from `target_store.default_goals` and `scheduling` falls back exactly
as for any other add.

Semantics that differ from an operator add:

- **Proximity-only dedup** replaces the slug-keyed same-object rule:
  rp searches all stored targets for the nearest row within
  `target_store.import.dedup_arcsec` (default 30″) of the received
  coordinates. A match that is still pending and import-owned
  (`!active && updated_by == source.kind`) is upserted in place —
  coordinates take the new value, `updated_at`/`updated_by` stamped,
  the provenance line refreshed; slug, `display_name`, and goals
  untouched; `created: false`. A match that is active,
  operator-edited, or operator-created is **never modified** — a new
  pending target is created beside it with a suffixed slug (Decision
  3's protection, enforced in rp, not bridge courtesy). No match
  creates.
- **The `catalog_ref`-match branch of slug allocation is never
  consulted**: two imports 15′ apart that both resolve to "NGC 7000"
  are two targets (mosaic panels), not one. An import never takes over
  an existing slug — a base-slug collision always suffix-allocates.
- **Naming by reverse cone-search** (`rp_catalog::nearest`, one query
  over the one logical catalog): entries carry a class, each class has
  its own acceptance radius — `naming_tolerance_arcmin` (default 10′)
  for deep-sky objects, `star_naming_tolerance_arcmin` (default 2′)
  for stars — and a DSO hit outranks any star hit regardless of
  separation (the tight star radius matches the faint-star-anchor
  gesture; flat nearest-wins would let field stars take names from
  nebula framings exactly where the nebulae live). A hit sets
  `catalog_ref` and denormalizes `object_type`/`magnitude`/
  `size_arcmin` exactly as a catalog add does. The display name is the
  plain name (`"NGC 7000"`) only when no stored target already claims
  that `catalog_ref` *and* the offset from the centroid is within
  `dedup_arcsec`; otherwise the offset form — `"NGC 7000 +8′E −4′N"`
  (East = Δα·cos δ, North = Δδ, each component rendered to 0.1′ with a
  trailing `.0` stripped and a component under 0.05′ omitted) — reads
  as *how this framing differs*. No hit falls back to the IAU-style
  truncated coordinate form `"J2059+4432"` (`Jhhmm±ddmm`), whose slug
  (`j2059p4432`, `p`/`m` for the sign) matches by construction. Names
  are initial values only: `display_name` stays freely editable, and
  existing rows are never retroactively renamed when a second framing
  or a wider catalog arrives. Catalog coverage bounds naming quality,
  never import correctness — identity, dedup, and slug allocation are
  pure coordinate proximity.

### Position angle

*(P2 of [planetarium-target-import.md](../plans/planetarium-target-import.md),
Decision 5.)* `Target` carries `position_angle_degrees` — the sky
position angle this target should be framed at, in degrees east of
north (`0.0 ≤ angle < 360.0`, finite; the same sky frame as
`move_rotator`'s `angle`). The **effective** angle is a three-layer
fallback resolved at read time, homed per optical train:

1. the target's own `position_angle_degrees` — an explicit angle
   freezes framing and is never reinterpreted;
2. the imaging train's
   `equipment.optical_trains[].default_position_angle_degrees`
   ([Optical Trains](#optical-trains)) — per-train, not global,
   because a camera's fixed mounting angle is a physical fact of one
   light path, and a rig with two rotator-less trains can carry two
   different angles;
3. `0.0` (north-up).

`get_next_target` resolves the fallback: it accepts an optional
`train_id` (an `equipment.optical_trains[]` id — unknown ids are an
error; omitted, layer two is skipped) and returns the effective angle
as a top-level `position_angle_degrees` field beside `exposure` (null
only when `target` is null). With a rotator in the train, the
`deep_sky` workflow threads that value through its blackboard into a
`move_rotator` call after slew/centering
([session-runner.md](session-runner.md) § `deep_sky.json`); on a
rotator-less train the config value documents physical reality and
nothing moves — the operator dials the same angle into the
planetarium's FOV indicator.

The field is operator-owned: `add_target` (non-import forms) and
`update_target` accept it, the [import form](#import-form-source)
rejects it, and imports always land with no angle (inherit). On
`update_target` an explicit `null` clears the field back to
inherit-the-train-default — the only `update_target` field with
explicit-null semantics, because "blank" (inherit) and "0.0"
(explicit north-up) must stay distinguishable for the [ui-htmx targets
inbox](ui-htmx.md#targets-inbox-targets)
(plan § P4 note).

### Capture-time target linkage

`rp` has no session-side "current target" — see [Capture Tool
Details](#capture-tool-details) for the full mechanism: `capture`'s
new optional `target` (slug) parameter, sourced from orchestrator
workflow state the same way `session-runner` already threads
`get_next_target`'s effective position angle through its blackboard
into a later `move_rotator` call (P2's precedent, [Decision
5](../plans/planetarium-target-import.md#decisions-fixed--settled-interactively-2026-07-22-revised-same-day-after-adversarial-review)).
This is what supplies the naming template's `{target}` token (§
Persistence) and the exposure document's `target` field (§ Exposure
Document).

### Target MCP tools

| Tool | Parameters | Returns | Description |
|------|-----------|---------|-------------|
| `add_target` | `catalog_ref` (name, resolved via `resolve_target`) *or* `display_name` + `ra_hours` + `dec_degrees` *or* `ra_hours` + `dec_degrees` + `source {kind, client, received_at}` (the [import form](#import-form-source)) — exactly one form; `active` (optional, default `true`; rejected with `source`), `goals[]` (optional — defaults to `target_store.default_goals` from config when omitted), `scheduling` (optional — field-for-field `SchedulingConstraints`; omitted fields fall back to `target_store.default_scheduling`), `notes` (optional; rejected with `source`), `position_angle_degrees` (optional — degrees east of north, `0.0 ≤ angle < 360.0`, see [Position angle](#position-angle); rejected with `source`), `grading` (optional — field-for-field `GradingThresholds`; omitted fields fall back to `target_store.default_grading`; rejected with `source`) | slug, created, target | Create or upsert a target per the slug-allocation and dedup rules above (proximity-only dedup and rp-side naming for the import form). `created` is `false` when the call resolved to an in-place edit of an existing row. Goal filter names are validated against the connected rig's configured filter roster (union of every `equipment.filter_wheels[].filters`; permissive when none are configured) (Decision 10) — an unknown name fails the call at add time, naming the offending goal, rather than failing at capture time mid-session |
| `get_target` | slug | target, progress | Fetch one target with derived progress (below) |
| `list_targets` | active_only (optional) | targets: [{...target fields, progress}] | List all targets, optionally filtered to `active == true` — the shape both `get_next_target`'s candidate set and the ui-htmx targets inbox read. Each element is the flattened target plus a `progress` field (not the `{target, progress}` nesting `get_target` uses) |
| `update_target` | slug, any subset of `display_name` / `ra_hours` / `dec_degrees` / `active` / `priority` / `scheduling` / `notes` / `position_angle_degrees` / `grading` | target | Edit fields in place. Does not touch the slug or on-disk frames. Setting `active: true` is how an operator (or the ui-htmx targets inbox) accepts a pending target into the rotation. `scheduling` and `grading`, when supplied, each replace the whole overrides object rather than merging field-wise; an explicit `null` on `grading` clears the override back to inherit-`default_grading`. Re-grading is free — thresholds are applied at read time, so tightening one immediately re-partitions `good`/`total` with nothing on disk renamed or moved. `position_angle_degrees` additionally accepts an explicit `null` to clear the per-target angle back to inherit-the-train-default (see [Position angle](#position-angle)) |
| `delete_target` | slug | deleted | Remove the target's plan row (`false` for an absent slug). Frames already captured under the slug are left untouched on disk — re-adding the same slug later silently re-adopts them for progress purposes; deleting a target with captured frames should generally prefer `update_target { active: false }` instead, to retire it without orphaning |
| `set_goals` | slug, goals[] | target | Replace the goal set atomically; same filter-roster validation as `add_target` |

The `target` objects these tools return are `Target`'s **derived** serde
serialization — there is no separate hand-maintained response shape. That
works because the plan value types serialize as their canonical wire
strings: `binning` as `"AxB"` (e.g. `"1x1"`), `exposure_duration` as a
humantime string (e.g. `"5m"`), `coord` as `{ra_hours, dec_degrees}`
(ADR-019). Input `goals[]` entries use the same
`{filter, binning, exposure_duration, desired_count}` shape (deserialized
via `GoalWire` first, for friendlier per-field errors than raw serde's), so
what a tool accepts is byte-for-byte what it returns. One consequence of
deriving: every stored field appears in responses — including `grading`,
which is `null` on a target that inherits `target_store.default_grading`
rather than overriding it.

### Plan schema and validation

The plan-data analogue of the `config.get` / `config.schema` /
`config.apply` protocol (ADR-019, and
[rp-vocabulary.md § Schema + validate protocol](../crates/rp-vocabulary.md#schema--validate-protocol-the-decision-1-role)):
two read-only MCP tools that let any surface discover the shape of plan
data and check a candidate against **the same rules the write tools
enforce**, without writing anything.

**MCP only — there is no REST counterpart.** rp's HTTP surface covers
service-level concerns (health, config, session lifecycle, documents,
events); plan data lives entirely on the MCP surface, so these tools add
no routes. `config.schema` remains the REST-side config analogue and is
unrelated to plan data.

| Tool | Parameters | Returns | Notes |
|---|---|---|---|
| `get_plan_schema` | `entity` | entity, schema | The JSON Schema for one plan-data entity, generated from the same types the write tools deserialize — so it cannot drift from what they accept |
| `validate_plan` | `entity`, `value` | valid, errors[] | Check `value` against every rule the corresponding write would apply. Writes nothing, touches no store, and never actuates |

`entity` is one of:

| `entity` | Shape | Validated the same way as |
|---|---|---|
| `target` | the `add_target` payload | `add_target` |
| `goals` | a `goals[]` array | `set_goals` / `add_target`'s `goals` |
| `naming_pattern` | `{file_naming_pattern?, directory_pattern?}` | config load |

`errors[]` entries are `{path, msg}` — the identical `FieldError` shape
`config-actions` returns, deliberately, so a surface renders a plan
error next to its field with the code it already has for driver config.
`path` is dotted and indexed into the submitted value
(`goals[1].binning`, `scheduling.min_altitude_degrees`, `ra_hours`).
`valid` is `errors.is_empty()`.

**The one property that matters.** These tools are worth having only if
they cannot disagree with the writers, so they do not re-implement any
rule: `validate_plan` and `add_target` / `set_goals` call the *same*
functions in `mcp::built_in::plan_validation`, which is the only place
the rules exist. The write tools render the first `FieldError` into
their existing error string; `validate_plan` returns the whole list
structured. A payload `validate_plan` accepts is therefore accepted by
`add_target`, and one it rejects is rejected with the same path —
pinned by BDD rather than left to convention.

Some rules deliberately sit outside this validator because they are
**facts about the world, not about the payload**: whether a slug already
exists (dedup and suffix allocation), whether a referenced slug is
present, and whether a `catalog_ref` resolves against the embedded
catalog. `validate_plan` reports a payload that is well-formed and
rule-compliant; it does not predict which slug an `add_target` would land
on, and says so rather than guessing.

Consolidating the rules closed a real gap in the writers rather than
merely exposing them: `add_target` and `update_target` never
range-checked their `grading` and `scheduling` parameters, so a negative
or `NaN` threshold was accepted as a per-target override while config
load rejected the identical value for `target_store.default_grading`.
Both now go through the shared validator.

### Progress derivation

Progress is computed on demand, never stored (full rules in
[`rp-targets.md` § Progress derivation](../crates/rp-targets.md#progress-derivation-the-actuals)):
a target's plan spans however many nights it takes to reach its
goals, so `rp` walks **every** night directory under a target's slug
(accumulating across the whole project, not one night), parses each
filename through the configured `file_naming_pattern` (§ Persistence)
to bucket frames by `(filter, binning, exposure_duration)`, then classifies each frame
good/rejected against its sidecar's grading section and the target's
effective `GradingThresholds` (its own overrides, field-wise over
`target_store.default_grading`). `get_target`/`list_targets` report, per
target, a list of `{filter, binning, exposure_duration, desired_count, good, total}` —
one entry per `AcquisitionGoal` — superseding the filter-only
`{completed, goal}` shape `get_target_status.progress` and
`get_session_progress` used to return, which could not distinguish two
goals that share a filter (e.g. `Ha` at two different exposure
lengths).

**When derivation is inert.** The whole scan is predicated on a
configured `session.file_naming_pattern`: without one `capture` still
succeeds and still stamps what each frame *is*, but it writes a flat
`<uuid8>.fits` carrying no target identity, so there is nothing on disk
to attribute. Every target then derives `0/desired_count` forever and a
goal-terminated session never ends on goals — it runs to `max_frames` or
dawn. That is a config-time mistake with a night-time symptom, so `rp`
**warns at startup** when the pattern is absent rather than letting it
pass silently. It is a warning and not a load failure because a rig may
legitimately image without goals; `target_store.default_grading` is the
stricter case and does fail the load (§ Configuration), since thresholds
cannot mean anything with no frames to grade.

**Which directories are walked.** The scan is driven by the same two
compiled templates `capture` renders (§ Persistence), so it finds
exactly the layout `capture` writes, whatever the operator configured.
`rp` walks `<data_directory>` to the depth of `directory_pattern`'s
`/`-separated segment count, parses each directory's data-directory-
relative path back through the *directory* template, and keeps the
ones whose `{target}` is this slug. Every `.fits` file in a kept
directory has its stem parsed through the *file* template. The frame's
sub-spec comes from the filename alone — `{filter}`, `{binning}`, and
`{exposure_duration}` are all *required* tokens of
`file_naming_pattern`, so they are always there — while its target and
frame type may come from either template, the filename winning when
both carry them. A frame is counted when:

- its `{target}` is this target's slug, **and**
- its frame type is `Light` — read from whichever template supplies
  `{frame_type}` (the default `directory_pattern` does). When neither
  pattern carries the token, frame type is unknown and every frame in
  the slug's directories counts; a rig configured that way cannot
  separate lights from calibration frames on disk, and its progress
  says so.

Directories and filenames that don't parse are skipped and
`debug!`-logged with the path — they count toward neither `total` nor
any goal and never fail the scan. An absent or empty slug directory
yields `total = 0` for every goal, so each reports `0/desired_count`:
an uncaptured filter is 0 %, not an error. Sidecar `.json` files are
ignored by the walk (they share a stem with their FITS file, so
counting both would double-count). The scan is `readdir` + regex —
no file opens at all unless grading is configured (below).

**Buckets to goals.** A bucket is keyed by the exact
`(filter, binning, exposure_duration)` triple, which is precisely an
`AcquisitionGoal`'s identity — and the store already rejects a goal set
carrying that triple twice (`rp-targets`' `validate_goals`, "duplicate
goal key"), so each bucket belongs to exactly one goal and the mapping
is total. Frames whose triple matches no goal are counted in neither
`good` nor `total`: progress is reported per goal, and a frame outside
every goal has no goal to progress. A goal that has been over-shot
reports the true count — `good` above `desired_count` is a finished
goal with frames to spare, not an error, and the planner clamps when it
computes a completion fraction rather than the scan hiding the surplus.

One normalization applies on the way in: a frame whose `{filter}` token
reads `NA` is matched to the **unfiltered** goal slot (`filter: ""`).
That is not a special case invented here — it is the inverse of what
`capture` already writes, which renders the literal `"NA"` for a train
with no filter wheel (§ Capture Tool Details). Without it, an OSC rig's
frames could never match its own unfiltered goals, and every plan on a
filter-wheel-less setup would sit at zero forever. A filter genuinely
named `NA` in a wheel's roster is indistinguishable from "no wheel"
under this rule, on disk and in progress alike — do not name a filter
`NA`.

**Good vs rejected.** For each counted frame `rp` reads the sidecar's
`grading` section and applies the target's **effective** thresholds
(`target.grading` field-wise over `target_store.default_grading`).
The section is a plugin section like any other (§ Plugin Sections) —
`rp` neither writes nor validates it; the grading plugin that measures
frames is a separate component and is not part of this service. `rp`
reads four optional numeric metrics, each paired with the threshold
that judges it:

| Sidecar metric | Threshold | Frame is rejected when |
|---|---|---|
| `hfr` | `max_hfr_pixels` | `hfr > max_hfr_pixels` |
| `star_count` | `min_star_count` | `star_count < min_star_count` |
| `eccentricity` | `max_eccentricity` | `eccentricity > max_eccentricity` |
| `snr` | `min_snr` | `snr < min_snr` |

```jsonc
"sections": {
  "grading": { "hfr": 2.31, "star_count": 1847, "eccentricity": 0.42, "snr": 38.6 }
}
```

**A frame is rejected only on evidence.** `good` counts every frame
that is not *demonstrably* rejected: a frame with no sidecar, no
`grading` section, an unparseable one, or simply no value for the
metric a threshold judges, counts as good. Only a metric that is
present *and* violates its effective threshold rejects a frame, and
any one violation is enough. This is what keeps progress meaningful
without a grading plugin installed — the alternative (ungraded means
not-good) would pin `good` at `0` forever on every rig that has no
plugin, so no plan could ever complete and every session would run
until dawn. It also makes the verdict dynamic in the direction the
design requires: nothing on disk is renamed or moved for rejection,
so raising a threshold re-admits frames it previously excluded.

When a target's effective thresholds are entirely empty — the default,
since `default_grading` is unset out of the box and a target's own
`grading` is `null` until an operator sets one — there is nothing any
sidecar could contradict, so `rp` skips sidecar reads altogether and
`good == total`. Configuring grading is what turns the per-frame
sidecar reads on, per target.

**Cost.** One `readdir` per night directory plus a regex per entry,
repeated per target on each read. At a realistic project size (a few
hundred nights of a few hundred frames) this is milliseconds, and
`get_next_target` performs it once per call for the active set rather
than once per target per check. Nothing is cached: a cache would have
to be invalidated by exactly the events (`capture` writing a frame, a
plugin writing a verdict, an operator editing a threshold or culling
files by hand) that the on-demand read already observes for free.

`record_exposure` no longer increments a counter — `capture` already
wrote the frame the derivation finds. It survives as the orchestrator's
per-frame progress readback. The planner's filter-batching tie-break
(§ Decision Logic bullet 4) reads the filter wheel itself — the one fact
the filesystem cannot supply, *which filter is in the wheel*, comes
from the device that holds it, not from a remembered frame. See its row
in [Planner Tools](#planner-tools).

### Configuration

Landed today:

```jsonc
"target_store": {
  "db_path": "/data/lights/targets.redb",      // default: <data_directory>/targets.redb
  "default_goals": [],
  "default_scheduling": {
    "min_altitude_degrees": 20.0,
    "min_moon_separation_degrees": 30.0,
    "max_moon_illumination_fraction": 1.0,     // 1.0 ⇒ no moon-brightness limit
    "meridian_window_hours": null              // null ⇒ no meridian window
  },
  "import": {
    "dedup_arcsec": 30.0,                      // proximity-upsert window; below any mosaic panel spacing
    "naming_tolerance_arcmin": 10.0,           // DSO-class cone radius; display only, never identity
    "star_naming_tolerance_arcmin": 2.0        // star-class cone; a star names a target only when no DSO is in its cone
  },
  "default_grading": {                         // optional; omitted ⇒ no frame is ever rejected
    "max_hfr_pixels": null,                    // setup-dependent; opt-in
    "min_star_count": 20,
    "max_eccentricity": 0.6,
    "min_snr": null
  }
}
```

The three `import` tunables drive the [`source` import
form](#import-form-source); each must be a finite positive number
(config load rejects anything else, naming the field).

`default_goals` is rp-owned policy (Decision 10): `add_target` applies
it when the caller supplies no `goals[]`, so a target created with no
explicit plan (e.g. a bare bridge import) still gets a sane default
rather than silently having none. `default_scheduling` is the value a
store-backed target's `None` `scheduling` fields fall back to in
`get_next_target` (Decision 9 — landed for `min_altitude_degrees`; the
other three fields are stored but their *enforcement* stays deferred,
per `rp-targets.md`'s MVP scope).

`default_grading` is the value a target's `None` `grading` override
fields fall back to (§ Progress derivation). Every field is optional
and `null` means "don't judge this metric"; an entirely absent or
empty `default_grading`, with no per-target override, means no frame
is ever rejected and `good == total`. Each supplied field must be a
finite number (config load rejects anything else, naming the field),
and `min_star_count` is a non-negative integer.

The frame scan `default_grading` feeds needs
`session.file_naming_pattern` to be configured (§ Persistence) — that
is what puts targets and sub-specs into the on-disk layout in the
first place. With no naming pattern configured, `capture` writes flat
`<doc_uuid_8>.fits` files that carry no target, so every goal reports
`0/desired_count` and the planner never exhausts a target. Setting
`default_grading` without `file_naming_pattern` is a config-load
error rather than a silent no-op.

The legacy `targets[]` config array is gone — a breaking, pre-1.0 hard
cutover. `Config` has `deny_unknown_fields`, so a stray `targets` key,
or an array shape under `target_store`, fails loudly at config load;
each target is (re-)added via the `add_target` MCP tool into the store.

## Dynamic Planner

The planner is a pure function exposed as MCP tools. Given current state,
it produces recommendations. The orchestrator calls planner tools to
decide what to do next — `rp` does not make workflow decisions.

### Planner Tools

| Tool | Parameters | Returns | Description |
|------|-----------|---------|-------------|
| `get_next_target` | train_id (optional — the imaging train, for the position-angle fallback and the filter-wheel read; unknown ids are an error) | target (nested `coord`), reason, exposure (nested `{filter, duration_secs}`, null when none), position_angle_degrees (the effective framing angle — target value → the named train's `default_position_angle_degrees` → `0.0`; null when target is null. See [Target Store → Position angle](#position-angle)) | Evaluate all active [Target Store](#target-store) rows and recommend the best target/filter. The filter-batching tie-break (§ Decision Logic bullet 4) reads the named train's sole filter wheel, or the rig's only wheel when no train is named |
| `get_target_status` | target_name | altitude, hour_angle, time_to_set, progress | Sky position and progress for a specific target |
| `get_meridian_status` | — | time_to_flip, side_of_pier | Time until meridian flip is needed |
| `record_exposure` | target, filter | target, filter, progress | Read back the target's derived progress after a frame. It does **not** increment anything — `capture` already wrote the frame the scan finds ([Target Store § Progress derivation](#progress-derivation)) — and records nothing (the filter-batching tie-break reads the wheel, § Decision Logic bullet 4). `progress` is the per-goal list below; an unknown target slug is still an error, so a mis-wired orchestrator fails loudly rather than silently losing frames |
| `get_session_progress` | — | progress | Full progress overview: target slug → the per-goal list below, for every active target-store row |

`get_target_status.progress`, `get_session_progress`, and
`record_exposure.progress` all carry the same per-goal shape as
`get_target`/`list_targets` — one entry per `AcquisitionGoal`, in goal
order:

```jsonc
[
  {"filter": "Ha", "binning": "1x1", "exposure_duration": "5m", "desired_count": 40, "good": 12, "total": 13},
  {"filter": "OIII", "binning": "1x1", "exposure_duration": "5m", "desired_count": 40, "good": 0, "total": 0}
]
```

This replaces the filter-keyed `{completed, goal}` map these tools
returned before the frame scan landed. The old shape could not
distinguish two goals sharing a filter, and its `completed` counted
only frames recorded in the current session; `good`/`total` count
every frame on disk for the target, across every night.

### Decision Logic (inside `get_next_target`)

The convenience tool delegates each numbered check to the named
primitive (or to the derived progress map for non-ephemeris
checks). Primitives are defined in the
[Primitive vs. Convenience MCP Tools](#primitive-vs-convenience-mcp-tools)
table.

`get_next_target` derives progress **once per call** for the active
target set (§ Progress derivation) and then decides against that
snapshot, so the decision logic itself stays a pure function of its
inputs — the filesystem is read at the tool boundary, not inside the
ranking.

1. Eliminate targets whose `compute_alt_az` altitude is below
   `min_altitude_degrees` (per-target value, falling back to the
   planner-wide `min_altitude_degrees` from config), or whose
   `compute_rise_set` set time leaves less than the
   `dawn_buffer_minutes` plus a single full exposure.
2. Among the survivors, prefer targets that are transiting —
   smallest absolute hour-angle from `compute_transit` against the
   current `get_local_sidereal_time` (highest altitude, best
   seeing).
3. Prefer targets with the least progress toward their integration
   goal (from the derived, on-disk progress — § Progress derivation).
4. Minimize filter changes: among the remaining ties, prefer the
   target whose next exposure uses the filter currently in the wheel.
5. Account for meridian flip timing — avoid starting a long
   exposure if `compute_meridian_flip` returns a `time_to_flip`
   shorter than the per-target `exposures[].duration_secs` plus a
   safety margin.
6. If no targets are viable, return a `WaitForTwilight`
   recommendation when the night has not started (the Sun is above
   astronomical dusk, −18°, and not rising — wait and re-ask), or
   `EndOfSession` when the night is over (the Sun is back above
   −18° and rising) or every target has met its integration goal.

The orchestrator decides when to call `get_next_target` — typically
after each exposure, after each target switch, or when conditions change.

> **v1 implementation status.** Five of the six bullets land at
> least partially in v1: the altitude half of bullet 1 (per-target /
> per-planner-default floor), bullet 2 (smallest-|HA| transit
> preference), bullets 3–4 (progress + filter tie-breaking, below),
> and bullet 6 in full — when no target survives, either **every**
> target has met its integration goal (all plans complete per the
> derived progress) and the answer is `EndOfSession`, or
> the Sun-elevation cut-off (astronomical dusk, -18°) separates a
> bright sky from `AllBelowMinAltitude` and the Sun's trend
> (sampled 60 s ahead) separates dusk from dawn: `WaitForTwilight`
> while the Sun is descending toward tonight's dark window (or
> holding level), `EndOfSession` once it is climbing out of it.
> A target whose plan is complete (every goal's `good` count has
> reached its `desired_count`) is **exhausted** and eliminated like a
> below-floor target; a target with no goals has no finite integration
> goal and is never exhausted. Exhaustion is judged on `good`, not
> `total`: a rejected frame does not advance a plan, so a night of
> poor seeing keeps the target in the rotation instead of retiring it
> on frames that will never be stacked.
> Bullets 3–4 break transit ties: targets whose |HA| is within
> 0.5 h of the best candidate's count as equally transiting (near
> culmination the altitude gain over half an hour of hour angle is
> negligible), and among them the smallest good-to-desired
> fraction wins (bullet 3; a target without goals counts as 0),
> then the target whose next exposure matches the filter in the wheel
> (bullet 4), then target-store list order. For bullet 4
> `get_next_target` reads the wheel at the tool boundary — the named
> imaging train's sole filter wheel (`train_id`; the sole-wheel rule
> `set_filter` applies), or the rig's only configured wheel when no
> train is named — with the same `Position` read `get_filter` makes,
> and passes the current filter name into the decision as an optional
> input. A rig without a wheel, a train with none or several, a
> disconnected wheel, a wheel mid-move or a failed read all leave the
> tie-break neutral (one `debug!` line): filterless rigs lose nothing,
> since their goals carry no filter. Reading the wheel rather than
> remembering the last recorded frame is what keeps the tie-break
> truthful after a focus run on another filter, a manual `set_filter`,
> or a wheel that homed on power-up.
> The recommendation carries the exposure plan progress-aware as a
> nested `exposure` object: `exposure.filter` and
> `exposure.duration_secs` are the recommended target's first
> **incomplete** plan entry in plan order (`exposure` is null when the
> target defines none) — 40 completed Luminance frames rotate the
> recommendation to the plan's Red entry.
> Documented v1 gaps:
>
> - **Bullet 1 set-time elimination** — `next_target` does *not*
>   currently check `compute_rise_set` against
>   `exposures[].duration_secs`. A target that rose ten minutes
>   before now and sets in five minutes can still be recommended;
>   the orchestrator must catch the short-set case and re-call
>   `get_next_target`.
> - **Bullet 5 (meridian-flip avoidance)** — satisfied *indirectly*:
>   a target whose transit was in the recent past has a large
>   positive HA and ranks lower than a target approaching transit,
>   so the smallest-|HA| pick tends to avoid imminent flips. The
>   planner does *not* check `time_to_flip` against
>   `exposures[].duration_secs` explicitly.
>
> A follow-up will close these gaps once session state is wired
> through.

### Target Definition

Targets are defined in the redb-backed store, not in config — see the
[Target Store](#target-store) section and its `add_target` MCP tool.

## What Survives an rp Restart

`rp` keeps no run state. There is no session registry, no state file
and no resume step: an `rp` restart mid-night (crash, power failure,
systemd restart) restores configuration and reconnects equipment, and
nothing else — because nothing else was `rp`'s to keep.

- **Runs** belong to the orchestrator that started them.
  `session-runner` persists a run manifest and its blackboard and
  resumes itself when it comes back, behind a reachable `rp` and a safe
  `get_safety_status` ([session-runner.md § Self-resume on
  startup](session-runner.md#self-resume-on-startup)); a run that was
  paused on an `rp` outage resumes on its own once `rp` answers again.
- **Progress** is derived from the frames on disk on every read
  ([Target Store § Progress derivation](#progress-derivation)), so a
  restarted `rp` reports the true count rather than zero and a plan
  spans nights without bookkeeping.
- **The planner's tie-break** reads the filter wheel (§ Dynamic
  Planner); there is no last-filter record to lose.
- **Cooling** is re-issued by the workflow: `start_cooldown` adopts a
  cooler still regulating at its rung without a thermal cycle (§ Camera
  Cooling → [Across an rp restart](#across-an-rp-restart)). `rp`
  touches no cooler at startup.
- **Safety** is re-read: when monitors are configured the poller's
  first pass sets the gate to reality and, on an unsafe reading,
  secures the equipment (§ Safety) before the listener serves, so an
  orchestrator that resumes against a fresh `rp` meets a gate that
  already reflects the sky.

There is deliberately no "conditions have changed" (daytime /
all-goals-met) check in `rp` — deciding whether the night is over is
planner work. A run resumed after dawn asks `get_next_target`, receives
`end_of_session` (the sun-trend rule — § Dynamic Planner) and completes
normally. An earlier revision kept a session registry in `rp` that
re-invoked the orchestrator after a restart and after a safety
interruption; it went with decision D6 of
[`mcp-sessionless.md`](../plans/mcp-sessionless.md) once orchestrators
started, paused and resumed their own runs.

## Safety

Safety monitoring is a top-level concern owned exclusively by `rp`. It
can override any operation, including cancelling any client's in-flight
gated call.

### SafetyMonitor Polling

`rp` polls every configured ASCOM Alpaca SafetyMonitor device
(`equipment.safety_monitors`, connected at startup like any other
device) at `safety.poll_interval` (humantime string, default `"10s"`).
When no monitors are configured the loop never starts and every tool
runs ungated. A monitor read is **fail-unsafe**: a device that is
disconnected or errors on `IsSafe` counts as unsafe, and the overall
state is safe only when *all* monitors report safe. Each per-monitor
transition emits a `safety_changed` event (`monitor`, `new_state`).
A monitor whose device session died (its service restarted, or it was
unreachable at `rp` startup) reads unsafe only until the reconnect
supervisor re-establishes the session (§ [Device Session
Recovery](#device-session-recovery)); the next poll then reads through
the new session and the unsafe state clears on its own.

On the overall safe → unsafe transition:

1. Close the safety gate: every **gated** tool call (§ [In-Flight
   Tool Calls](#in-flight-tool-calls)) is answered with the
   `SafetyUnsafe` JSON-RPC error while conditions remain unsafe. The
   gate closes at the *first* unsafe reading of the poll pass, before
   the remaining monitors are read — a device read that hangs to its
   timeout must not keep gated tools dispatching while an unsafe
   reading is already in hand; the steps below run once the pass
   completes. Nothing else is refused — every ungated tool keeps answering, so a
   waiting client can observe state (`get_safety_status`), secure
   hardware, and spend the hour on darks, bias, panel flats and
   cooling. There is no session to close — the transport is
   session-less (§ [MCP Server](#mcp-server)) and `rp` keeps no record
   of who is driving; a client's next gated call is refused, and an
   orchestrator such as `session-runner` pauses its run in-process and
   waits for the safe transition, keeping its persisted state
   ([session-runner.md § Safety Behavior](session-runner.md#safety-behavior)).
2. Cancel every in-flight **gated** tool call, and every in-flight
   `capture`, through the in-flight registry (§ [In-Flight Tool
   Calls](#in-flight-tool-calls)): each cancelled body issues its
   stop-class counterpart (a slew aborts, an exposure aborts, an opening
   cover halts) and answers its caller with the tool error
   `cancelled: safety`. Every other ungated body already in flight — a
   `park`, a `close_cover`, a `set_filter` — runs to completion. `rp`
   waits up to `CANCEL_ACK_TIMEOUT` (3 s) for the cancelled bodies to
   acknowledge before the hardware steps below run, so a cancelled
   slew's `AbortSlew` cannot land on top of the park.
3. Abort in-progress exposures on all connected cameras (best-effort).
   Step 2 already aborted every exposure a registered `capture` was
   driving; this catches a camera left exposing with no body to answer
   for it, since the park that follows would ruin the frame either way.
4. Stop guiding through the configured guider service (best-effort;
   a confirmed stop emits `guide_stopped` with `reason: "safety"`, a
   failed one is logged and skipped so the park below still runs).
5. Park the mount (best-effort, fire-and-forget: the Alpaca `Park`
   is issued and logged, but `rp` does not block on `AtPark` —
   Sentinel's watchdog owns escalation if the mount never gets
   there).

The hardware steps run in that order deliberately: the mount must not
move under an exposing camera or an active guide loop, and a cancelled
body's abort must not land on the park that follows.

On the overall unsafe → safe transition:

1. Open the safety gate: gated tools answer again.
2. Nothing else. The per-monitor `safety_changed` event
   (`new_state: "safe"`) is the only signal; `rp` re-invokes nobody
   and restores nothing, because it kept nothing. An orchestrator
   that paused on the unsafe transition watches for that event (or
   polls `get_safety_status`), confirms an overall safe reading, and
   decides how to resume — verify pointing, re-acquire guiding,
   continue from its own persisted progress.

There is no debounce knob: the gate follows each poll. Flapping
conditions produce repeated close/open cycles, each of which is safe by
construction; whether to wait out a flap before resuming is the
orchestrator's call (`session-runner` confirms a safe reading before
every resume, and its re-entrancy contract makes resume idempotent).

### In-Flight Tool Calls

Every `tools/call` is entered in an in-flight registry for the lifetime
of the call — keyed by an `rp`-internal serial, because JSON-RPC request
ids are only unique per client — together with the tool's **class** and
a cancellation handle derived from the request's own rmcp token. rmcp
cancels that token when the caller's HTTP request goes away before its
response (the connection dropped) or the caller sends
`notifications/cancelled`, so a caller that is gone cancels its own
call through the same handle, whatever the class.

Two classes, no default: every tool in the catalog names one, and a
unit test fails the build when a new tool forgets. A tool is **gated**
when it moves the mount towards the sky or exposes the optics to it. A
tool that stops or secures — `park`, `abort_slew`, `stop_guiding`,
`pause_guiding`, `close_cover`, `calibrator_off`, `start_warmup` — is
never gated, even where it moves the mount, because it is what the
transition itself does. Everything else is **ungated**: every read, the
target-store writes, `plate_solve`, and the indoor actuators (`capture`,
`set_filter`, `move_focuser`, `move_rotator`, `calibrator_on`,
`start_cooldown`, `sync_mount`, `auto_focus`, `refocus_train`) — darks,
bias, panel flats and cooling are what an unsafe hour is for (tenet 1). `unpark` and
`set_tracking` move nothing by themselves but are the door to motion,
so the sequence fails at its first step.

| Class | Tools |
|---|---|
| Gated | `slew`, `center_on_target`, `unpark`, `set_tracking`, `dither`, `start_guiding`, `resume_guiding`, `open_cover` |
| Ungated | every other tool in the catalog |

The class drives exactly two behaviours, and nothing finer: which
calls the safety gate refuses while conditions are unsafe
(§ SafetyMonitor Polling, step 1), and which in-flight calls the unsafe
transition cancels (step 2) — plus `capture`, which stays ungated
(darks and bias while unsafe are tenet 1) but whose in-flight body is
cancelled on the transition all the same: that is the abort-exposure
step delivered through the body, so the caller learns its frame died
for safety rather than seeing a bare hardware error. A compound ungated
tool with an exposure in flight (`auto_focus`, `refocus_train`) sees
that hardware error instead (step 4) and fails with it.

**The safety gate.** The class is checked at tool dispatch, *after*
the call is registered — the registration is what the unsafe
transition sweeps, and the enforcer closes the gate before it sweeps,
so a call racing the transition is either refused here or cancelled by
the sweep, never run. A gated tool called while conditions are unsafe
is never dispatched: the request is answered HTTP 200 with a JSON-RPC
error in the range the MCP spec leaves to servers —

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "error": {
    "code": -32010,
    "message": "safety: conditions are unsafe",
    "data": { "reason": "safety", "monitor": "weather-watcher" }
  }
}
```

`code` `-32010` is `SafetyUnsafe`; `data.monitor` names an unsafe
monitor (the first in id order when several are, `null` if none can be
named). Every MCP client library surfaces this as a structured error,
and `rp-mcp-client` maps it to `McpCallError::SafetyStopped`. An
ungated tool is dispatched as usual whatever the conditions, and a name
that is not in the catalog is left to the router's own "tool not found"
answer. There is no HTTP-level gate: `/mcp` accepts every request,
`server/discover` and `tools/list` work while unsafe, and the REST
surface is never gated. Plugin-provided tools are gated unless their
registration says otherwise (§ [Tool Provider
Registration](#tool-provider-registration)).

**Operator overrides.** The built-in table is a default, not a verdict:
a rig with a sealed dome may want `open_cover` ungated, a rig with a
fragile focuser may want `auto_focus` gated. `safety.gate` in the
config carries two lists (§ Configuration):

```json
"safety": {
  "poll_interval": "10s",
  "gate": {
    "gated": ["auto_focus"],
    "ungated": ["open_cover"]
  }
}
```

Each name must exist in the catalog, and a name in both lists is an
error; both are rejected at load and by `PUT /api/config`, naming the
offending entry (`safety.gate.gated.0`). The override applies to the
gate and to cancellation alike — there is one class. The effective
table is logged once at `info!` at startup and reported by
`get_safety_status` (`gated: [...]`), so an operator can see what their
config did without reading the source.

**`get_safety_status`.** For a client that does not consume SSE, the
tool reports the state the gate is acting on:

```json
{
  "overall": "unsafe",
  "since": "2026-09-03T02:41:07Z",
  "monitors": [
    { "id": "weather-watcher", "state": "unsafe", "since": "2026-09-03T02:41:07Z" }
  ],
  "gated": ["center_on_target", "dither", "open_cover", "resume_guiding",
            "set_tracking", "slew", "start_guiding", "unpark"]
}
```

`since` is when that reading last changed (process start until it
does); a monitor whose read failed reports `unsafe` like any other
unsafe reading. With no monitors configured `overall` is `safe` and
`monitors` is empty. The `safety_changed` event stays the push signal.

**Cancellation contract.** A cancelled body stops within one poll tick
(100 ms), issues its stop-class counterpart where one exists, and
answers its caller with the tool error `cancelled: <reason>`, where the
reason is `safety` (the unsafe transition) or `client disconnected`
(the caller's connection dropped before the response, or it sent
`notifications/cancelled`).
The operation's `*_failed` event carries the same text as its `error`.
A body still waiting for the [mount motion gate](#mount-motion-gate)
returns without moving anything.

| Cancelled body | Stop-class counterpart |
|---|---|
| `slew`, and the slews inside `center_on_target` | `AbortSlew` |
| `capture`, and the captures inside `auto_focus` / `center_on_target` | `AbortExposure` |
| `move_focuser`, and the moves inside `auto_focus` / `refocus_train` | focuser `Halt` |
| `move_rotator` | rotator `Halt` |
| `open_cover` | `HaltCover` |
| `start_guiding` | stop guiding — the half-started loop is undone |
| `set_filter`, `close_cover`, `calibrator_on`, `calibrator_off`, `park`, the guide-metric waits inside `auto_focus` | none: the wait ends and the device finishes on its own. A park is never aborted — a mount half-way to its park position is safer left going there, the same reasoning as `park`'s timeout |
| `dither` | none — PHD2 has no dither abort, so the guider finishes its settle on its own. The mount is still being pulsed towards the new lock position, though, so the body's [motion-gate](#mount-motion-gate) permit moves to a detached holder that releases it when the settle RPC ends (bounded by the settle timeout plus 15 s, 90 s when none was given) |
| `unpark`, `set_tracking` | none: each is a single driver call raced against the handle. A handle already cancelled when the body runs means the call is never issued; a cancel landing mid-call drops the request, and the transition's own park re-secures the mount either way |

Long-running bodies emit `notifications/progress` while they poll
(client feedback for a `progressToken`-bearing caller); that emission
is independent of cancellation.

### Sentinel Watchdog Integration

Sentinel is extended beyond safety monitoring to serve as an operation watchdog
and supervisor for the entire system. It connects to `rp`'s real-time event
stream (`/api/events/subscribe`) and monitors operation deadlines. The stream
connection also serves as a health signal — if `rp` itself crashes or hangs,
the disconnection is an immediate trigger for Sentinel to attempt recovery.

#### Monitored Operations

| Operation | Starts on event | Expected completion | Timeout = |
|-----------|----------------|--------------------|----|
| Exposure | `exposure_started` | `exposure_complete` | `max_duration_ms` from the `exposure_started` envelope (rp-computed advisory: `duration + camera.readout_time_estimate + 30 s` readout headroom; rp does not itself enforce it) |
| Slew | `slew_started` | `slew_complete` | `max_duration_ms` from the `slew_started` envelope (rp-computed: `(distance / rate + settle) × 3`, floored at `MIN_SLEW_DEADLINE`) |
| Park | `park_started` | `park_complete` | `max_duration_ms` from the `park_started` envelope (rp-computed worst-case traverse: `(180° / rate + settle) × 2`, floored at `MIN_PARK_DEADLINE`) |
| Move focuser | `move_focuser_started` | `move_focuser_complete` | `max_duration_ms` from the `move_focuser_started` envelope (rp-computed: `(\|target − current\| / steps_per_sec) × 2`, floored at `MIN_FOCUSER_DEADLINE`) |
| Focus | `focus_started` | `focus_complete` | configurable max focus time |
| Guide settle | `guide_started` | `guide_settled` | `max_duration_ms` from the `guide_started` envelope (the resolved settle timeout + the guider service's 10 s backstop grace; omitted when no settle timeout is configured per-call or in the `equipment.mount.guiding` block). `dither_started` → `dither_settled` carries the same deadline shape |
| Centering | `centering_started` | `centering_complete` | `max_duration_ms` from the `centering_started` envelope (rp-computed advisory outer-loop deadline: `max_attempts × (duration + centering.solve_time_estimate + centering.slew_overhead_estimate)`; per-iteration ops carry their own deadlines) |

#### Corrective Actions

When a deadline expires without the expected completion event:

1. **Health check** — Sentinel pings the relevant Alpaca service endpoint
   to determine if it is responsive.
2. **Responsive but stuck** — Sentinel commands an abort via the device's
   Alpaca API (e.g., `PUT camera/0/abortexposure`). Notifies `rp` to re-plan.
3. **Unresponsive** — Sentinel executes the configured restart command for
   that service (e.g., `systemctl restart qhyccd-alpaca`). After restart,
   notifies `rp` to reconnect and resume.
4. **Notification** — Sentinel sends a push notification (Pushover or other
   configured notifier) describing the failure and corrective action taken.

The restart commands are configured per service, not hardcoded. Sentinel does
not know how to restart any specific service — it just executes the configured
command.

#### Recovery Flow

```
Sentinel detects: exposure_started 300s ago, no exposure_complete
  │
  ├─► Health check camera driver endpoint
  │     │
  │     ├─► Responsive → PUT abortexposure → notify `rp`
  │     │
  │     └─► Unresponsive → run restart command → wait for service
  │           │
  │           └─► Service back → notify `rp` → `rp` reconnects;
  │                                                 the orchestrator resumes
  └─► Send push notification describing what happened
```

## API Layer

`rp`'s client surface is MCP (Tenet 8): every capability a UI, plugin, or
external client drives is an MCP tool on `/mcp`. The HTTP/REST routes below
are **not** a second surface that mirrors those tools — they carry only what
cannot ride MCP: raw image bytes, the SSE event stream, plugin completion
callbacks, and config (which must stay reachable while conditions are
unsafe), plus a few operational reads (`/health`, equipment status).
Whatever REST serves, it stays a dumb pipe — no application logic.

### REST Endpoints

The router serves only the unmarked routes below. A bullet marked
*(planned)* for a client **action** — device connect/disconnect,
planner introspection — is a leftover REST sketch: per Tenet 8 that
capability lands as an MCP tool, not a new REST route. Nothing here mirrors
the target-store CRUD tools; those are MCP-only (§ Target Store).

#### Equipment
- `GET /api/equipment` — live connection status per configured device. The
  response mirrors the config's equipment shape: ten fixed keys
  (`cameras`, `filter_wheels`, `cover_calibrators`, `focusers`,
  `safety_monitors`, `switches`, `rotators`, `observing_conditions`,
  `domes` — arrays of `{ "id", "connected" }` — and `mount`, a
  `{ "connected" }` object or `null`). The `id` is the operator-supplied
  config id; the mount is singular and has none. Device *addresses and
  settings* are not repeated here — they live in the config, readable via
  `GET /api/config`, and a UI joins the two by `id`.
- Runtime device connect/disconnect is **not** a REST route: the registry is
  built once at startup, sessions are kept alive by the reconnect supervisor
  (§ [Device Session Recovery](#device-session-recovery)) — so `connected`
  reflects the live session state, not the startup snapshot — and if an
  operator-facing connect/disconnect is ever exposed it lands as an MCP
  tool, not a REST endpoint (Tenet 8).

#### Configuration
- `GET /api/config` — the effective configuration, secrets redacted, plus
  CLI-override-pinned paths. Same `{ config, overrides }` body as the
  drivers' `config.get`, as plain REST (no Alpaca envelope). See
  [`config-actions.md`](config-actions.md) "REST transport".
- `GET /api/config/schema` — JSON Schema for the config plus editability
  tiers (`{ schema, locked_fields, read_only_fields }`; `server.port` is
  hard read-only — the self-lockout guard).
- `PUT /api/config` — validate and atomically persist a full Config
  (body = the Config JSON; response = the `config.apply` classification
  body). **rp has no in-process reload**: every changed field is reported
  in `restart_required[]` with `status:"ok"`, and the persisted file takes
  effect on the next rp start. Validation failure → HTTP 200
  `status:"invalid"` + field-level `errors[]`, file untouched; a malformed
  JSON body → HTTP 400; a body over axum's default 2 MiB request limit →
  HTTP 413 (a valid config is a few KiB). The config endpoints are covered by the
  server-wide auth/TLS and, like every REST route, are never safety-gated
  — configuration must stay editable while the system is unsafe.

#### Runs
- There are no run routes. Runs are started, listed and stopped at the
  orchestrator (`session-runner`'s `/runs`, `polar-align`'s `/runs` and
  `/status`); `rp` has no notion of a session (§ [Orchestration](#orchestration)).
- Planner **introspection** (why it chose the current target, upcoming
  decisions) is *(planned as an MCP tool — Tenet 8)*, not a REST route.

#### Documents
- `GET /api/documents` — list recent exposure documents *(planned)*
- `GET /api/documents/{id}` — full document with all sections. Returns
  the same JSON written to the sidecar. Resolves through the cache with
  on-disk fallback; returns `404` only when neither cache nor disk has
  the document. See
  [Document Resolution](#document-resolution).
- `POST /api/documents/{id}/sections` — add/update a section (plugin
  endpoint). Requires the document be resolvable; persists the sidecar
  atomically. *(planned — plugins deliver sections through
  `POST /api/plugins/{id}/complete` today)*

#### Images
- `GET /api/images/{document_id}` — image metadata (width, height, bitpix,
  FITS path, exposure document link, in-cache flag)
- `GET /api/images/{document_id}/pixels` — raw pixel data in
  `application/imagebytes` (ASCOM Alpaca ImageBytes wire format). See
  [Image and Document Cache](#image-and-document-cache). Consumers
  wanting FITS read the file directly from the path in the exposure
  document.

#### Plugins
- `POST /api/plugins/{id}/complete` — event plugin completion callback
  (status, optional `correction`). The `{id}` is the delivery's
  `event_id`.

#### MCP
- `/mcp` — MCP server endpoint (streamable HTTP transport). Workflow
  plugins connect here as MCP clients to discover and call tools.

#### System
- `GET /health` — health check
- `GET /api/events/subscribe` — SSE (Server-Sent Events) stream of real-time events

### Real-Time Stream

`GET /api/events/subscribe` is a [Server-Sent Events](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events)
stream (`Content-Type: text/event-stream`) carrying every event the
[Event System](#event-system) emits — the same envelopes delivered to plugin
webhooks, plus stream-control frames. Any consumer that needs live events
connects here: UIs for rendering state, and the Sentinel operation watchdog
for tracking deadlines and rp liveness. It is the primary mechanism for
passive consumers — push updates without the webhook ack/completion protocol.

**Frame format.** Each event is one SSE frame:

- `id:` — the envelope's `event_seq` (the monotonic total order; doubles as
  the `Last-Event-ID` replay cursor).
- `event:` — the event type (e.g. `slew_started`).
- `data:` — the full [Event Envelope](#event-envelope) as JSON.

A `:keep-alive` comment is sent every 15 s so idle connections survive
middleboxes.

**Reconnect & replay.** rp retains the most recent 512 events in memory. A
reconnecting client sends its last seen `event_seq` — via the standard
`Last-Event-ID` header (the browser `EventSource` API sets this automatically)
or the explicit `?last_event_id=<seq>` query parameter (the header wins if
both are present). The server replays every retained event after that cursor,
oldest first, then resumes the live tail. The replay→live handoff is
exactly-once: an event is delivered via replay or live, never both, never
neither.

**Gaps.** If the cursor predates the retained window (the client was gone long
enough that its next expected event was evicted), the stream leads with a
`stream_gap` event — `event: stream_gap`, no `id`,
`data: {"event":"stream_gap","requested_after":<cursor>,"oldest_available":<seq>}`
— so the consumer knows it lost history. Sentinel treats a `stream_gap` as a
trigger to escalate any operation it was tracking when the gap occurred.

**Slow consumers.** A consumer that falls more than the broadcast buffer
(256 events) behind the live tail is sent a final `stream_gap`
(`{"event":"stream_gap","lagged":<n>}`) and disconnected, rather than being
allowed to back up the in-process channel. It recovers by reconnecting with
its `Last-Event-ID` (replayed from the 512-event history, or told of the gap
if it fell too far behind).

**Liveness.** When rp shuts down it ends all in-flight subscribe streams, so a
dropped stream is itself a signal: Sentinel treats the disconnection as an
immediate trigger to attempt recovery (see
[Sentinel Watchdog Integration](#sentinel-watchdog-integration)).

Authentication and TLS, when configured, apply to this endpoint exactly as to
every other route.

## Configuration

All configuration is in a single JSON file. `rp serve --config <path>`
(or the `rp --config <path>` shorthand) names it explicitly; when omitted
— including a bare `rp`, which serves (that is what the packaged
`rusty-photon-rp` unit runs) — the path resolves to the platform default
(`~/.config/rusty-photon/rp.json` on Linux,
`%PROGRAMDATA%\rusty-photon\rp.json` on Windows) via
`rusty-photon-config`, and a minimal runnable scaffold (no equipment,
default port, `session.data_directory` under the platform-dependent state
directory — `/var/lib/rusty-photon/rp/` on Linux,
`~/Library/Application Support/rusty-photon/rp/` on macOS,
`%PROGRAMDATA%\rusty-photon\rp\` on Windows)
is written on first start if the file is absent. The default has to be
writable by the account the packaged service runs as, because opening the
target store creates it (§ [Target Store](#target-store)): Linux gets that
from the unit's systemd `StateDirectory=` and Windows from `LocalSystem`'s
access to `%PROGRAMDATA%`, while macOS — where `brew services` runs as the
invoking user, who cannot write `/var/lib` — puts session data beside the
config under its own platform root. An explicit `--config`
naming a missing file stays a hard error. Equipment is listed with Alpaca
connection details. Plugins register their webhook URLs and command endpoints.
Every block (top-level `Config` and each equipment/service sub-config) rejects
unknown keys at deserialize (`deny_unknown_fields`), so a typo or a key
removed by a schema change fails loudly at load instead of being silently
ignored.

`rp doctor [--config <file>] [--json]` diagnoses this service's own config
read-only without starting it — see
[doctor.md §Per-service doctors](doctor.md). Unlike `serve`, it never writes
the first-start scaffold.

The running config is readable and editable over REST — `GET /api/config`,
`GET /api/config/schema`, `PUT /api/config` (see
[Configuration endpoints](#configuration) above): rp implements the shared
config-actions protocol ([`config-actions.md`](config-actions.md)) with
`ApplyDisposition::Restart`, so an applied change is persisted atomically to
this file and reported as `restart_required` — it takes effect on the next rp
start. This is what the `ui-htmx` equipment page edits.

The `mount` field is singular: exactly one mount is the typical
deployment. Piggyback rigs share that one mount across multiple optical
trains — `cameras`, `focusers`, and `filter_wheels` stay plural for the
trains (declared in `equipment.optical_trains`, see
[Optical Trains](#optical-trains)); `mount` stays singular. Multi-mount
support is in
[Future Considerations](#future-considerations). The optional
`mount.guiding` block configures the guider rp-managed service —
guiding is mount-scoped, so it nests here rather than at top level.
The retired shapes from before the optical-trains model — a top-level
`guider` block, `cameras[].focal_length_mm`, and the `camera_id`
back-references on `focusers[]` / `filter_wheels[]` — are rejected at
load by `deny_unknown_fields` like any other unknown key (pre-1.0 hard
cutover; existing configs need a one-time hand edit).
`mount.settle_after_slew`
is applied by `slew` after the mount reports `Slewing == false`; per-call
`settle_after` on `slew` overrides this value (including `"0s"` to skip
when the config sets a non-zero default). `mount.slew_rate_arcsec_per_sec`
(default `7200` = 2°/s, a conservative slow-stepper rate) feeds the
predictive slew deadline; set it per-rig for a tighter bound. It must be a
finite positive number — a bad value is rejected at config load.
`focuser.steps_per_sec` (default `500`, a conservative slow rate) feeds the
predictive `move_focuser` deadline the same way — likewise a finite
positive number rejected at load otherwise.
`cameras[].cooler_targets_c` must hold unique integers on the 5 °C grid
(−40 … +15); off-grid values are rejected at load with the offending
field named (see [Camera Cooling](#camera-cooling)).

The top-level `ca_cert` names a PEM CA certificate `rp` trusts for
every outbound HTTPS connection it makes as a client — Alpaca devices
(`equipment.*[].alpaca_url`), the plate-solver service, the guider
service, event plugins' `webhook_url`, and tool providers'
`mcp_server_url`. An observatory
runs one self-signed CA (doctor's D6
provisioning), so this is a single rp-level setting rather than a
per-device or per-service one; `doctor --fix` writes it automatically
once the CA exists (`services/doctor/src/provision/mod.rs`
`CA_ONLY_WIRING_SERVICES`). Setting it makes it the client's **only**
trusted root (`tls_certs_only`, ADR-002) — it replaces, not adds to,
the platform trust store, so a public-CA `https://` target becomes
unreachable alongside the observatory CA. Omitted (the default), only
the platform trust store applies, so an `https://` target signed by
the observatory CA fails certificate verification regardless of the
device's `auth` credentials — see
[ASCOM Alpaca Devices](#ascom-alpaca-devices).

The optional `server.advertised_url` (e.g.
`"https://rp.rig.example.org:11115"`) names the URL clients reach
`rp` at when it is not derivable from the listener; it must be an
`http://` or `https://` URL and is rejected at load otherwise. Its
effect is to admit that host to the MCP endpoint's `Host` allowlist,
which is how a name `rp` cannot derive (a reverse proxy, an mDNS alias)
is made acceptable to the endpoint; unset (the default), the allowlist
holds the listener-derived names — a wildcard `bind_address` admits the
system hostname; see [MCP Server](#mcp-server). The whole
`server` block is `rusty-photon-server-config`'s
`AdvertisingServerConfig` — the shared plain-HTTP shape plus this
field — which `rp` uses directly rather than defining a copy of, so
the shape `rp` accepts is by construction the one
`rusty-photon-doctor` validates it against (`class = "advertising"`
in `services/rp/pkg/doctor.toml`).

The `site` block is required for the ephemeris and planner tools
(`compute_alt_az`, `get_twilight`, `get_next_target`, …); when present
it is validated against the ASCOM mount on connect — see
[Site Validation Against the ASCOM Mount](#site-validation-against-the-ascom-mount).
A config without `site` loads cleanly and `rp` runs, but those tools
return a structured "site not configured" error.

```json
{
  "session": {
    "data_directory": "/data/lights",
    "file_naming_pattern": "{target}_{filter}_{binning}_{frame_number}_{exposure_duration}_fpos_{filter_position}_{sensor_temp}"
  },
  "site": {
    "latitude_degrees": 47.6062,
    "longitude_degrees": -122.3321
  },
  "equipment": {
    "reconnect_interval": "30s",
    "cameras": [
      {
        "id": "main-cam",
        "name": "Main Imaging Camera",
        "alpaca_url": "https://localhost:11120",
        "device_type": "camera",
        "device_number": 0,
        "cooler_targets_c": [-10, 5],
        "gain": 100,
        "offset": 50,
        "readout_time_estimate": "8s",
        "auth": {
          "username": "observatory",
          "password": "secret"
        }
      },
      {
        "id": "guide-cam",
        "name": "Secondary / Wide field Camera",
        "alpaca_url": "http://localhost:11121",
        "device_type": "camera",
        "device_number": 0,
        "cooler_targets_c": [],
        "gain": 200,
        "offset": 30
      }
    ],
    "optical_trains": [
      {
        "id": "main",
        "purpose": "imaging",
        "focal_length_mm": 1000.0,
        "default_position_angle_degrees": 254.0,
        "devices": ["flat-panel", "main-focuser", "main-fw", "falcon", "main-cam"],
        "auto_focus": {
          "duration": "3s",
          "step_size": 100,
          "half_width": 1000,
          "min_area": 4,
          "max_area": 500
        }
      },
      {
        "id": "guide",
        "purpose": "guiding",
        "focal_length_mm": 200.0,
        "devices": ["main-focuser", "guide-focuser", "guide-cam"],
        "auto_focus": { "step_size": 50, "half_width": 500,
                        "frames_per_step": 3 }
      }
    ],
    "mount": {
      "alpaca_url": "http://localhost:11122",
      "device_number": 0,
      "settle_after_slew": "3s",
      "slew_rate_arcsec_per_sec": 7200,
      "guiding": {
        "url": "http://localhost:11130",
        "timeout": "90s",
        "settle_pixels": 0.8,
        "settle_time": "10s",
        "settle_timeout": "60s",
        "dither_pixels": 5,
        "recalibrate_above_deg": 5.0,
        "focus_watch": { "window": 10, "degrade_ratio": 1.25,
                         "cooldown": "10m",
                         "escalation_deadline": "10m" },
        "auth": {
          "username": "observatory",
          "password": "secret"
        }
      }
    },
    "focusers": [
      {
        "id": "main-focuser",
        "alpaca_url": "http://localhost:11113",
        "device_number": 0,
        "min_position": 0,
        "max_position": 100000,
        "steps_per_sec": 1200
      },
      {
        "id": "guide-focuser",
        "alpaca_url": "http://localhost:11113",
        "device_number": 1,
        "auth": {
          "username": "observatory",
          "password": "secret"
        }
      }
    ],
    "filter_wheels": [
      {
        "id": "main-fw",
        "alpaca_url": "http://localhost:11123",
        "device_number": 0,
        "filters": ["Luminance", "Red", "Green", "Blue", "Ha", "OIII", "SII"]
      }
    ],
    "safety_monitors": [
      {
        "id": "weather-watcher",
        "alpaca_url": "http://localhost:11111",
        "device_number": 0
      }
    ],
    "cover_calibrators": [
      {
        "id": "flat-panel",
        "alpaca_url": "http://localhost:11125",
        "device_number": 0
      }
    ],
    "switches": [
      {
        "id": "ppba",
        "name": "Pegasus PPBA",
        "alpaca_url": "http://localhost:11112",
        "device_number": 0
      }
    ],
    "rotators": [
      {
        "id": "falcon",
        "alpaca_url": "http://localhost:11118",
        "device_number": 0
      }
    ],
    "observing_conditions": [
      {
        "id": "ppba-weather",
        "alpaca_url": "http://localhost:11112",
        "device_number": 0
      }
    ],
    "domes": []
  },
  "plate_solver": {
    "url": "http://localhost:11131",
    "timeout": "60s",
    "default_search_radius_deg": 3.0,
    "auth": {
      "username": "observatory",
      "password": "secret"
    }
  },
  "imaging": {
    "cache_max_mib": 1024,
    "cache_max_images": 8
  },
  "centering": {
    "solve_time_estimate": "30s",
    "slew_overhead_estimate": "10s"
  },
  "cooling": {
    "poll_interval": "10s",
    "plateau_window": "2m",
    "plateau_threshold_c": 0.5,
    "tolerance_c": 1.0,
    "max_cooler_power_pct": 90,
    "regulation_margin_c": 3.0,
    "max_cooldown": "20m",
    "warmup_step_interval": "2m",
    "warm_target_c": 10.0
  },
  "plugins": [
    {
      "name": "image-analyzer",
      "type": "event",
      "webhook_url": "http://localhost:11140/webhook",
      "subscribes_to": ["exposure_complete"],
      "barrier_gates": ["slew", "set_filter"]
    },
    {
      "name": "cloud-backup",
      "type": "event",
      "webhook_url": "http://localhost:11141/webhook",
      "subscribes_to": ["exposure_complete", "safety_changed"]
    },
    {
      "name": "ml-quality-classifier",
      "type": "tool_provider",
      "mcp_server_url": "http://localhost:11150/mcp",
      "requires_tools": ["compute_image_stats"]
    },
    {
      "name": "calibrator-flats",
      "type": "tool_provider",
      "mcp_server_url": "https://localhost:11170/mcp",
      "auth": { "username": "observatory", "password": "secret" },
      "gate": { "train_flats": "none", "take_flats": "none", "get_flat_training": "none" },
      "requires_tools": [
        "get_train_info", "get_camera_info", "capture", "compute_image_stats",
        "set_filter", "get_cover_state", "close_cover", "open_cover",
        "calibrator_on", "calibrator_off"
      ]
    }
  ],
  "target_store": {
    "db_path": "/data/lights/targets.redb",
    "default_goals": [],
    "default_scheduling": {
      "min_altitude_degrees": 20.0,
      "min_moon_separation_degrees": 30.0,
      "max_moon_illumination_fraction": 1.0,
      "meridian_window_hours": null
    }
  },
  "planner": {
    "min_altitude_degrees": 20,
    "dawn_buffer_minutes": 30,
    "prefer_transiting": true,
    "minimize_filter_changes": true
  },
  "safety": {
    "poll_interval": "10s",
    "gate": {
      "gated": [],
      "ungated": []
    }
  },
  "server": {
    "port": 11115,
    "bind_address": "0.0.0.0",
    "auth": {
      "username": "observatory",
      "password_hash": "$argon2id$v=19$m=19456,t=2,p=1$..."
    }
  },
  "ca_cert": "/etc/rusty-photon/pki/ca.pem"
}
```

## Module Structure

```
services/rp/src/
  main.rs               CLI entry point (clap + tracing)
  lib.rs                Public API, ServerBuilder + BoundServer, module declarations
  config.rs             Configuration types + load_config()
  error.rs              AppError enum (thiserror)

  # Core domain
  target.rs             rp-targets store wiring: opens RedbTargetStore
                        at startup (target_store.db_path), slug allocation,
                        dedup/upsert policy (§ Target Store)
  cooling.rs            Camera-cooling controller behind the
                        start_cooldown / start_warmup tools:
                        setpoint-ladder selection, adoption, hold,
                        warm-up ramp (§ Camera Cooling)
  motion_gate.rs        MotionGate: the mount readers-writer gate
                        (§ Mount Motion Gate) — exclusive for
                        slew/dither, shared for imaging-train
                        captures, mount_motion_pending emission
  guiding_watch.rs      Guide Focus Watch (§ Guide Focus Watch):
                        polls the guider's metrics window while
                        guiding, baseline/degrade/escalation state,
                        guide_focus_degraded / guide_focus_escalation
                        emission — events only, never actions

  # Equipment layer
  equipment/
    mod.rs              EquipmentRegistry: per-device entries, status
    supervisor.rs       Reconnect supervisor (§ Device Session Recovery):
                        per-interval health check + session re-establish,
                        plus the tool-provider lane (mcp/providers.rs)
    alpaca.rs           Generic Alpaca client (reqwest-based)
    camera.rs           Camera device wrapper (expose, abort, cooler, readout)
    mount.rs            Mount wrapper (slew, park, flip, tracking, side of pier)
    focuser.rs          Focuser wrapper (move, temperature)
    filter_wheel.rs     Filter wheel wrapper (set/get position)
    safety_monitor.rs   SafetyMonitor wrapper (poll is_safe)
    cover_calibrator.rs CoverCalibrator wrapper (cover open/close, calibrator on/off)
    trains.rs           TrainModel: the derived optical-train coupling
                        model (§ Optical Trains) — graph validation +
                        the derivation queries (focuser-for-camera,
                        AF sequence, invalidations, focal length)

  # Services (non-Alpaca integrations, backing built-in MCP tools)
  # Per-service HTTP clients live in workspace crates following the
  # `crates/rp-*` convention. plate_solve's MCP tool wiring lives in
  # mcp/built_in/plate_solve.rs with transport/types in
  # crates/rp-plate-solver; the guiding tools live in
  # mcp/built_in/guider.rs with transport/types in crates/rp-guider.
  services/
    mod.rs              Service trait, service manager

  # Safety enforcement
  safety.rs             SafetyMonitor polling loop, the safety status
                        the gate reads, session interrupt/resume,
                        in-flight cancellation, exposure abort,
                        guiding stop, mount park

  # Planning (exposed as MCP tools — see Planning and Ephemeris)
  # Math and catalog data live in workspace crates rp-ephemeris and
  # rp-catalog; this sub-tree is the MCP-tool wrapping plus the
  # decision logic that composes the primitives.
  planner/
    mod.rs              Module root; tool registration helpers
    primitives.rs       MCP wrappers for the 10 ephemeris primitives
    catalog.rs          MCP wrapper for resolve_target (over rp-catalog)
    convenience.rs      Derived-`Serialize` view helpers for
                          get_target_status, get_meridian_status, and the
                          progress views (get_next_target has no helper —
                          it serializes decision.rs's recommendation type
                          directly)
    decision.rs         The decision logic from §"Dynamic Planner",
                          parameterised by an `Ephemeris` impl + an
                          explicit `now` so tests are deterministic

  # Event system
  events/
    mod.rs              Event types, EventBus
    webhook.rs          Webhook delivery (fire-and-forget HTTP POST)

  # MCP tool system
  # Pattern: each tool category gets its own #[tool_router(router = X,
  # vis = "pub")] impl block on McpHandler in its own file. McpHandler::new
  # merges the per-category routers via `+`, and a single
  # #[tool_handler(router = self.tool_router)] impl ServerHandler glues
  # them into rmcp's transport. Adding a new tool category = one new
  # file in built_in/, one line in built_in/mod.rs, one `+` in
  # handler::McpHandler::new — no edits to existing categories.
  mcp/
    mod.rs              Module root: declares submodules, holds the
                          shared private macros (tool_success!, tool_error!,
                          resolve_device!) exposed via `pub(crate) use`,
                          and the explicit
                          `#[tool_handler(router = self.tool_router)]
                           impl ServerHandler for McpHandler {}` block.
                          Re-exports `McpHandler`.
    handler.rs          The McpHandler struct (state fields plus
                          `tool_router: ToolRouter<Self>`),
                          `new()`/`with_planner_config()`/
                          `with_plate_solver()`/`with_guider()`/
                          `with_providers()`.
                          `new()` merges per-category routers via
                          `Self::tool_router_<category>() + …`;
                          `with_providers()` adds one proxy route per
                          provider tool.
    providers.rs        Tool-provider aggregation (§ Plugin-Provided
                          Tools): dial + discover at startup, the merge
                          and `requires_tools` checks, the proxy body
                          (forwarding, progress relay, cancellation),
                          the outage error, and the reconnect
                          supervisor's provider lane.
    internals.rs        Private/`pub(crate)` helpers shared across
                          categories: do_capture, do_move_focuser_blocking,
                          persist_capture_artifact,
                          read_mount_hints_for_plate_solve,
                          resolve_mount, the `*_via_document`/`*_via_path`
                          dispatch helpers, and small private types
                          (Resolved*Params, BackgroundOutcome,
                          PollIdleError) plus free fns clip_outcome,
                          detect_outcome, star_to_json,
                          poll_slewing_until_idle.
    tests.rs            Current home for the full mcp test module
                          (~3,400 lines including six mock-device
                          types, six EquipmentRegistry builders, and
                          the assert_tool_error / ok_text / ok_json
                          helpers). A planned follow-up (see below)
                          will distribute these tests across each
                          built_in/<category>.rs file matching the
                          imaging/ convention, and split the shared
                          mock-device fixtures into a sibling
                          test_support.rs module.
    built_in/
      mod.rs            Declares the per-category submodules.
      camera.rs         CaptureParams, CameraIdParams + capture +
                          get_camera_info.
      imaging.rs        6 imaging param structs + compute_image_stats,
                          measure_basic, estimate_background,
                          detect_stars, measure_stars, compute_snr.
      filter_wheel.rs   SetFilterParams, FilterWheelIdParams +
                          set_filter, get_filter.
      cover_calibrator.rs CalibratorIdParams, CalibratorOnParams +
                          get_cover_state, close_cover, open_cover,
                          calibrator_on, calibrator_off (calibrator_id
                          or train_id addressing; `trains` on results).
      trains.rs         GetTrainInfoParams + get_train_info (a read over
                          the train model; no device touched).
      focuser.rs        FocuserIdParams, MoveFocuserParams +
                          move_focuser, get_focuser_position,
                          get_focuser_temperature.
      mount.rs          SlewParams, SyncMountParams, SetTrackingParams,
                          GetTrackingParams, GetMountPositionParams,
                          ParkParams, UnparkParams, GetParkStateParams,
                          AbortSlewParams + the 9 mount tools.
      auto_focus.rs     AutoFocusToolParams, RefocusTrainParams +
                          auto_focus + refocus_train tools +
                          AutoFocusAdapter (binds the imaging::tools::auto_focus
                          traits to the handler's primitives) + the
                          guide-train PHD2-metric sweep (median HFD
                          per position over the guider metrics
                          window).
      rotator.rs        MoveRotatorParams, RotatorPositionParams +
                          move_rotator, get_rotator_position
                          (rotator_id / train_id addressing, blocking
                          IsMoving poll on moves, the
                          rotate-while-guiding ladder around
                          guiding-train moves).
      plate_solve.rs    PointingHint, PlateSolveParams + plate_solve
                          tool.
      guider.rs         6 guider param structs + the 6 guiding tools
                          (start_guiding, stop_guiding, dither,
                          pause_guiding, resume_guiding,
                          get_guiding_stats), proxying to the guider
                          service via crates/rp-guider.
      planner.rs        13 planner param structs + 10 ephemeris
                          primitive tools + 3 convenience tools
                          (get_target_status, get_next_target,
                          get_meridian_status).
      targets.rs        Target CRUD tools (add_target, get_target,
                          list_targets, update_target, delete_target,
                          set_goals) over crates/rp-targets'
                          TargetStore (§ Target Store).
    # Planned follow-up: distribute the centralized tests.rs into
    # per-category `#[cfg(test)] mod tests` blocks inside each
    # built_in/<category>.rs (matching the imaging/ test-colocation
    # convention) and extract the shared mock-device fixtures and
    # registry builders into a sibling test_support.rs module.
    #
    # Planned (not in tree yet):
    # - aggregator.rs    Connects to plugin MCP servers, proxies their
    #                    tools. Lands when the third-party plugin
    #                    protocol does.

  # Imaging (pure analysis kernels and the compositional tools that bind them)
  # Async, I/O, and on-disk layout live in `persistence/` so the analysis
  # path stays unit-testable without a runtime.
  imaging/
    mod.rs              Module root: re-exports the flat `imaging::*` API
                          shape that callers use, regardless of which
                          submodule a symbol is defined in.
    analysis/           Pure single-purpose kernels — generic over Pixel,
                          take ArrayView2, no I/O, no async.
      mod.rs
      pixel.rs          Pixel trait (impls for u16 and i32) for generic analysis
      stats.rs          Pixel statistics (median, mean, min, max ADU)
      background.rs     Sigma-clipped background estimation
      stars.rs          Star detection + centroiding (4-connectivity BFS)
      hfr.rs            HFR / HFD radial flux accumulation
      fwhm.rs           2D Gaussian fitting via rmpfit
      snr.rs            Per-star + median SNR (CCD-equation approximation)
    tools/              Compositional analyzers — bind multiple kernels
                          together to answer one MCP-tool-shaped question.
                          Pure functions; the MCP wrapper in `mcp.rs`
                          resolves pixels and serializes results.
      mod.rs
      measure_basic.rs  measure_basic tool: background + stars + hfr
      measure_stars.rs  measure_stars tool: per-star photometry + PSF fit

  # Persistence (FITS I/O, image+document cache, exposure-document storage)
  persistence/
    mod.rs              Module root: re-exports CachedImage / ImageCache /
                          ExposureDocument / write_fits_u16 / write_fits_i32 etc.
    document.rs         ExposureDocument struct, atomic sidecar JSON
                          persistence (write_sidecar_at: stage to .tmp →
                          rename). Document storage and lookup are
                          mediated by the unified Image and Document
                          Cache (`persistence/cache.rs`).
    cache.rs            ImageCache: CachedPixels enum (U16 | I32),
                          Arc<CachedImage> holding pixels + document
                          together, LRU eviction over combined memory
                          footprint, readdir+DOC_ID disk fallback.
    fits.rs             FITS read/write via rp-fits (writes BITPIX=16 for
                          16-bit sensors, BITPIX=32 fallback when max_adu
                          exceeds u16::MAX; reads normalise to i32; embeds
                          the document UUID in DOC_ID).

  # Post-capture pipeline
  pipeline/
    mod.rs              Pipeline orchestrator: dispatch async tasks after capture
    save.rs             Write FITS to final location, create sidecar JSON

  # HTTP layer — the technical-exception surface (Tenet 8); the
  # client surface is MCP. One flat file, no api/ package.
  routes.rs             Axum router mounting /mcp (rmcp streamable
                        HTTP, session-less — § MCP Server) alongside
                        the HTTP routes: /health, /api/equipment, /api/config
                        [+ /schema], /api/plugins/{event_id}/complete,
                        /api/documents/{id}, /api/images/{id}[/pixels]
                        (Alpaca ImageBytes), /api/events/subscribe (SSE).
                        No /api/targets — target CRUD is MCP-only
                        (§ Target Store); no application logic here.

  # I/O abstractions
  io.rs                 Traits for HTTP client, clock, filesystem (testability)
```

## Testing Strategy

Testing follows the conventions in `docs/skills/testing.md`.

### Unit Tests

- **Planner tools**: Given a target list, progress, and sky state, assert
  correct target/filter selection. Pure function, easy to test exhaustively.
- **Safety enforcement**: Assert correct behavior on unsafe transitions
  (transition detection, in-flight cancellation, the per-tool safety
  gate and its `SafetyUnsafe` error).
- **Document**: Serialization round-trips, section merging, atomic persistence.
- **Configuration**: Deserialization, validation, defaults.
- **Config-time validation**: Missing tools, conflicting plugins, circular
  dependencies.
- **Sky calculations**: covered by `rp-ephemeris`'s own
  reference-value tests against Astropy-generated values
  (alt/az / transit / rise/set / sun / moon / twilight). `rp`'s
  unit tests don't re-test the math; they assert the MCP wrappers
  deserialise inputs, dispatch through the trait, and serialise
  outputs correctly. `planner/decision.rs` is unit-tested with a
  mock `Ephemeris` impl over hand-rolled fake positions.

### BDD Tests (Cucumber)

Behavioral specifications for `rp`'s responsibilities:

- Safety override (unsafe cancels in-flight gated work and gates the
  tools; safe lifts the gate — the end-to-end pause and resume of a
  run against a real engine lives in
  `services/session-runner/tests/features/recovery.feature`)
- Config rejection of the removed orchestrator surface (D11)
- Tool-provider aggregation (`tool_providers.feature`, against the
  `bdd-infra` stub provider): provider tools in the catalog, a proxied
  call and its result, a colliding name failing startup, a safety stop
  cancelling an in-flight provider tool at the provider, an outage
  answering an error with the catalog unchanged and the supervisor
  bringing the provider back, and the registration's `gate` opt-out
- MCP tool validation and safety guardrails
- Event delivery to webhook endpoints
- Power failure recovery (`startup_recovery.feature`: derived
  progress survives an rp restart on disk — see § What Survives an rp
  Restart; the end-to-end restart of a run against a real engine lives
  in `services/session-runner/tests/features/recovery.feature`)

Note: orchestration workflow tests (capture loops, target switching,
meridian flips) belong to the orchestrator plugin, not to `rp`. For
example, end-to-end flat calibration scenarios live in
`services/calibrator-flats/tests/` and spawn `rp` via the `rp-harness`
feature of `bdd-infra`. Each new workflow plugin owns its own BDD
suite rather than adding scenarios here.

#### Prerequisites

BDD tests require the [ASCOM Alpaca Simulators (OmniSim)](https://github.com/ASCOMInitiative/ASCOM.Alpaca.Simulators)
binary. The test harness discovers the binary in this order:

1. `OMNISIM_PATH` env var — full path to the binary
2. `OMNISIM_DIR` env var — directory containing the binary
3. `ascom.alpaca.simulators` on `PATH`

To install locally, download the appropriate release binary for your
platform, extract it, and either add its directory to `PATH` or set one
of the environment variables above. In CI, the
`.github/actions/install-omnisim` composite action handles this
automatically.

**CI pins a patched fork**, not upstream: the action defaults to
[`ivonnyssen/ASCOM.Alpaca.Simulators` `v0.5.0-467.2`](https://github.com/ivonnyssen/ASCOM.Alpaca.Simulators/releases/tag/v0.5.0-467.2).
The `467.x` releases (issue #467) add the `--multi-instance` flag (467.1,
skips upstream's machine-global single-instance mutex) and the
`OMNISIM_SETTINGS_DIR` env var (467.2, re-roots the profile store per
instance on every platform — the default store is not redirectable on
Windows or macOS). The harness always spawns OmniSim with both, so **the
fork is a hard requirement for BDD runs now**, local ones included: every
test process (parallel suites, `rp:bdd` shards) gets a private simulator
on its own port with a private profile store. It also carries the
`326.x` series of `TelescopeHardware` fixes
for the `center_on_target` slew-state hang/flake (issues #326, #319):
326.1/.2 put the slew-engine writers and the
`IsSlewing`/RA/Dec/`AtPark`/`SlewState` readers under `hardwareLock`;
326.3 disposes the per-`Init()` slew timer (it leaked one live timer on
each per-scenario "restart to clean state" reset, accumulating tick sources
that raced the single static slew engine) and resets the slew state —
including the `slewing` flag — under `hardwareLock`. Those addressed the
locking/leak races but **not** the underlying wedge: 326.4 fixes it
(issue #319). The real cause is geometric — a GEM goto whose
shortest-path primary rotation crosses the hour-angle software limit
(`180 + hourAngleLimit`) gets undone by `CheckAxisLimits` every tick, so
the slew never finishes and `IsSlewing` stays `true` forever.
`center_on_target`'s sync-then-slew-to-near-the-same-coords triggers it
for the `RA < 180` off-target row at certain sidereal times (hence the
intermittent CI flake). 326.4 makes `DoSlew` take the limit-avoiding
rotation to the *same* target (pier side unchanged, so ConformU stays
clean) and adds a no-progress guard. Reverting the action's `repo` and
`version` inputs to upstream
[`v0.5.0`](https://github.com/ASCOMInitiative/ASCOM.Alpaca.Simulators/releases/tag/v0.5.0)
is no longer a one-line change: upstream has no `--multi-instance`, so
the `bdd-infra` spawn path (and the parallel/sharded BDD scheduling
built on it) would have to be reverted too. For local runs, use the
pinned fork — upstream `v0.5.0` lacks `--multi-instance` (every BDD
suite fails at OmniSim spawn) and still carries the #326 races and the
sidereal-time-gated #319 wedge.

#### Graceful Shutdown and Coverage

BDD tests spawn `rp` as a child process. For LLVM coverage data to be
captured from the child process, two conditions must be met:

1. **Graceful shutdown via SIGTERM.** LLVM coverage writes `.profraw`
   files through an `atexit` handler, which only runs on clean process
   exit. `SIGKILL` skips `atexit`, so no coverage data is written.
   `lib.rs` handles `SIGTERM` (and `Ctrl-C`) via `tokio::signal` to
   trigger a clean shutdown.

2. **Explicit `stop()` before `Drop`.** The `ServiceHandle` (from the
   shared `bdd-infra` crate) is created with `kill_on_drop(true)` as a
   safety net against leaked processes. However, when `Drop` fires, it
   sends `SIGTERM` immediately followed by `SIGKILL` from `kill_on_drop`
   — too fast for the process to flush. The cucumber `after` hook in
   `bdd.rs` calls `handle.stop()` explicitly, which sends `SIGTERM` and
   waits for the process to actually exit (up to 5 seconds) before the
   `ServiceHandle` is dropped.

Coverage is collected by `bazel coverage` (the sole coverage source). The BDD
target instruments both the test binary and the spawned `rp` binary; `bdd-infra`
sets `LLVM_PROFILE_FILE=$COVERAGE_DIR/<pkg>-%8m.profraw` on each spawned child so
its `.profraw` lands in `COVERAGE_DIR`, and a vendored `rules_rust` patch adds the
spawned binaries as extra `-object`s to `llvm-cov export` so their coverage is
emitted. See `.bazelrc` (`--config=coverage`) and `crates/bdd-infra/src/lib.rs`.

### Integration Tests

- MCP tool tests with mock equipment
- Tool provider aggregation (proxy plugin-provided tools)
- Event delivery to webhook endpoints

### I/O Abstractions

All external I/O (HTTP calls, filesystem, clock) goes through traits defined in
`io.rs`. Tests inject mocks to verify behavior without real devices or network.

## Future Considerations

Items explicitly out of scope for the initial implementation:

- **Distributed plugins** — plugins on remote machines accessing FITS files
  over the network
- **Plugin marketplace / registry** — discovery and installation of third-party
  plugins
- **Multiple mounts** — the current design assumes one mount; extending to
  multiple mounts is a separate concern
- **Dome control** — the `domes` equipment kind covers roster membership and
  connectivity status only (§ Equipment Integration); actual dome behavior
  (open/close, sync-to-scope) is still out of scope
- **Mosaic planning** — multi-panel target definitions
- **Ambient-aware cooldown preflight** — skipping obviously unreachable
  cooler rungs (and warning early) from an ObservingConditions ambient
  reading. The `observing_conditions` equipment kind (§ Equipment
  Integration) now exists, satisfying the prerequisite; the preflight
  feature itself is still out of scope. Ambient stays a preflight
  optimization, never the rung decider (§ Camera Cooling).
- **Abort-on-unreachable cooling** — an opt-in knob to end the session
  when no dark-library rung is reachable, instead of the default
  proceed-uncooled-with-warning (§ Camera Cooling).
- **Automated dark-library capture per rung** — a cloudy-night
  orchestrator job, sibling of calibrator-flats.

Note: flat/dark frame automation is no longer out of scope — it can be
implemented as a calibration orchestrator plugin without changes to `rp`.

# calibrator-flats -- Flat-Field Tool Provider

## Overview

`calibrator-flats` is a tool provider ([rp.md § Plugin-Provided
Tools](rp.md#plugin-provided-tools)) that owns one thing neither `rp`
nor a `session-runner` document can hold: **the exposure time and panel
brightness that produce a 50 % flat for each optical train and filter**,
learned once at dusk and reused every night after. It serves three tools
through `rp`'s catalog — `train_flats`, `take_flats`,
`get_flat_training` — backed by a [redb](https://crates.io/crates/redb)
store keyed by train and filter, and drives the rig by calling `rp`'s
own primitive tools as an MCP client. A night document takes flats with
one `take_flats` call and a `train_id`.

The service has no run surface of its own: MCP is the only way in, and
`/health` stays for systemd, sentinel and `doctor`. It is the first
first-party tool provider; the decision record is the
[calibrator-flats-provider plan](../plans/calibrator-flats-provider.md).

### Tenets

1. **Target 50 % well depth.** A flat's median should sit at half the
   camera's `max_adu`. The fraction is fixed — it is not a config field
   and it is not stored.
2. **Train once, take every night.** `train_flats` runs the proportional
   exposure search per filter and writes a record when it converges;
   `take_flats` reads the record and captures at exactly that duration
   and brightness. Nothing is re-derived on a night that is only taking
   flats.
3. **A stale record is untrained.** A record whose `camera_id`,
   `max_adu`, binning, `gain` or `offset` no longer match what `rp`
   reports for the train's camera is refused, naming the filter and the
   field that changed. Age alone is not a criterion.
4. **A bad request actuates nothing.** `take_flats` checks every
   requested filter against the store *before* touching the cover, the
   panel or the wheel. The first thing a refused call moves is nothing.
5. **Put things back.** A cover that was open when a tool started is
   reopened at the end; one that was closed — or read as `Moving`,
   `Unknown` or `Error` — stays closed, because covered is the state
   that protects the optics. The panel is always turned off. Cleanup
   runs on every exit: success, error and cancellation alike.
6. **Verification is advisory.** Every flat is measured after the fact
   and a median outside the tolerance band is a warning in the result,
   never a retry and never a failure: a dying lamp, a powered-off panel,
   an aged panel and a wrong filter in the drawer all look the same from
   here, and the operator decides whether to retrain.
7. **No actuation at startup.** The provider answers `tools/list` with
   no `rp` in sight and connects to `rp` lazily, per tool call. Nothing
   moves until a client asks for flats.

## Architecture

`calibrator-flats` is both an MCP **server** (`/mcp`, registered in
`rp` as a `type: "tool_provider"` plugin) and an MCP **client** of `rp`
(the standard `rp-mcp-client`, [ADR-017](../decisions/017-standard-mcp-client-construction.md)).
A client of `rp` — a `session-runner` document, an operator's MCP
client — calls `take_flats` on `rp`; `rp` proxies the call here; the
tool body connects back to `rp` and drives the cover, panel, wheel and
camera through `rp`'s primitives; the result goes back through `rp`
verbatim, with progress relayed and cancellation forwarded.

```
  session-runner / operator        rp (equipment gateway)          calibrator-flats (tool provider)
  ┌───────────────────┐            ┌───────────────────────┐        ┌────────────────────────────┐
  │ tools/call        ├───────────►│ proxy: take_flats     ├───────►│ /mcp  take_flats           │
  │  take_flats       │            │  (gate: none)         │        │  ├─ FlatStore (redb)       │
  │  {train_id, count}│◄───────────┤ progress + result     │◄───────┤  └─ McpClient ─┐           │
  └───────────────────┘            │                       │        └───────────────┼────────────┘
                                   │ get_train_info        │◄───────────────────────┘
                                   │ get_camera_info       │   set_filter, capture, compute_image_stats,
                                   │ close_cover, open_cover, calibrator_on, calibrator_off, get_cover_state
                                   └───────────────────────┘
```

There is no cycle at startup: `rp` dials the provider once to discover
its tools, and the provider does not need `rp` to answer `tools/list`.
`rp`'s packaged unit orders itself `After=` this one so a cold boot
finds the provider up before `rp` dials it.

### Port

11170 (configurable). `/mcp` and `/health` share the `server` block —
the same `server.tls` and `server.auth` guard both, so `rp`'s
registration `auth` is the same observatory credential every other
first-party client presents.

### Transport

Stateless streamable HTTP with JSON responses (`legacy_session_mode =
false`, `json_response = true`), the stack `rp` itself serves. The
`Host` allowlist is rmcp's loopback defaults plus the machine's hostname,
the explicit bind address, and every non-loopback interface address on a
wildcard bind — the same derivation `rp` uses, so `rp` can dial the
provider by hostname or LAN address, not only through `localhost`.

## Tools

All three tools are registered ungated (`"gate": "none"`) in `rp`'s
config. Flats are what an unsafe hour is for: the cover is closed and the
roof can be too. The optics only get exposed through `open_cover`, which
`rp` gates on its own, so the safety property holds by composition — see
[Cleanup](#cleanup-and-cancellation) for what a refused reopen looks
like. Nothing in the provider reads safety state.

Every tool takes a `train_id`. The provider resolves the train through
`rp`'s `get_train_info` — the terminal camera, the sole filter wheel
with its filter names, the cover calibrator — and never carries a
per-train filter list of its own. A train without a cover calibrator, or
without a camera, is a tool error naming the train before anything
moves.

### Filter selection

A `filters` argument names wheel positions by their configured names.
On a train with a filter wheel, `filters` defaults to every name the
wheel reports, in position order; a name the wheel does not have is an
error naming it and listing what the wheel has. On a filterless train
(a one-shot-color camera), `filters` must be absent — passing it is an
error — and the single capture group stores under the train id alone.

### `train_flats {train_id, filters?, brightness?}`

Learns the flat timing for a train. `brightness` is the ladder's
starting level; default the device maximum (whatever `calibrator_on`
reports when asked for none).

1. Reads the cover state, closes the cover, lights the panel.
2. Per filter: moves the wheel, runs the [exposure search](#exposure-search)
   with the brightness ladder from `initial_duration`, and **writes a
   record when it converges**. An unconverged search writes nothing and
   leaves any earlier record for that filter in place.
3. Turns the panel off; reopens the cover only if it started open.

Result:

```jsonc
{
  "train_id": "main",
  "trained": [                        // one record per converged filter
    { "train_id": "main", "filter": "Luminance", "duration": "1s 200ms",
      "brightness": 127, "median_adu": 32100, "max_adu": 65535,
      "bin_x": 1, "bin_y": 1, "gain": 100, "offset": 10,
      "camera_id": "main-cam", "trained_at": "2026-09-05T19:02:11Z" }
  ],
  "unconverged": [                    // per failure; nothing was written
    { "filter": "Blue", "best_duration": "30s", "median_adu": 9800 }
  ],
  "cover_restored": true,
  "warnings": []
}
```

**Partial success is a normal result**, not a tool error: a caller reads
`unconverged`. Progress: one `notifications/progress` tick per search
iteration, `progress` counting iterations across filters, `total`
absent (the ladder makes it unknowable), message naming the filter, the
exposure and the measured median.

### `take_flats {train_id, count, filters?}`

Captures `count` flats per filter at the trained timing.

1. **Checks the store before touching anything.** Every requested filter
   must have a record, and that record must match the live camera facts
   (`camera_id`, `max_adu`, `bin_x`, `bin_y`, `gain`, `offset` — read
   through `get_camera_info`). The call fails naming every untrained or
   stale filter and, for a stale one, each field that changed; `rp`
   sees no actuation at all. `count` must be at least 1.
2. Reads the cover state, closes the cover.
3. Per filter: moves the wheel, `calibrator_on` at the record's
   `brightness`, captures `count` frames as `frame_type: "Flat"` at the
   record's `duration`.
4. Turns the panel off; reopens the cover only if it started open.

Every frame is verified per [Verification](#verification) without
holding up the next exposure: the statistics of frame *n* are computed
while frame *n + 1* exposes.

Result:

```jsonc
{
  "train_id": "main",
  "filters": [
    { "filter": "Luminance", "duration": "1s 200ms", "brightness": 127,
      "frames": 20,
      "out_of_range": [ { "image_path": "/data/flats/…_003.fits", "median_adu": 21000 } ] }
  ],
  "total_frames": 20,
  "cover_restored": true,
  "warnings": [ "…_003.fits: median 21000 ADU is outside 32767 ± 10 % — panel dimmer than when trained?" ]
}
```

Progress: one tick per frame, `progress` = frames captured so far,
`total` = `count` × number of filters, message naming the filter and
the frame index.

`take_flats` never writes the store (no write-back, no aging model —
plan D5, O5).

### `get_flat_training {train_id, filter?}`

The records for a train, or for one filter, each judged against the
live camera exactly the way `take_flats` judges it, so an operator can
see *why* a filter counts as untrained. Reads `get_train_info` and
`get_camera_info`; touches no device.

```jsonc
{
  "train_id": "main",
  "camera": { "camera_id": "main-cam", "max_adu": 65535, "bin_x": 1, "bin_y": 1,
              "gain": 120, "offset": 10 },
  "records": [
    { "record": { …FlatRecord… }, "status": "stale",
      "stale": [ "gain changed from 100 to 120" ] },
    { "record": { …FlatRecord… }, "status": "trained", "stale": [] }
  ]
}
```

A train with no records answers an empty `records` list. An unknown
train is `rp`'s own error (`train not found: …`), relayed.

### Errors

Tool errors (`isError: true`, one text block) name the cause:

| Condition | Message shape |
|-----------|---------------|
| `rp` cannot be reached at `mcp_server_url` | `rp at <url> is unreachable: …` |
| Unknown train, train without camera or calibrator | `train not found: x` (rp's) / `train 'x' has no cover calibrator` |
| `filters` on a filterless train | `train 'x' has no filter wheel; do not pass filters` |
| Unknown filter name | `filter 'Ha' is not on train 'x' (wheel 'main-fw' has: Luminance, Red, Green, Blue)` |
| A wheel with no configured names, or an empty `filters` list | `the filter wheel 'main-fw' of train 'x' has no configured filters` / `filters must name at least one filter` |
| `count` of 0 | `count must be at least 1` |
| `take_flats` on untrained / stale filters | `train 'x' is not ready for take_flats: Luminance untrained; Red stale (gain changed from 100 to 120) — run train_flats first` |
| An `rp` tool failed mid-run (device error, aborted exposure) | the `rp` message, after cleanup |
| The caller cancelled | `cancelled: <reason>`, after cleanup |

A JSON-RPC error from the provider (a malformed argument object) is
relayed by `rp` as that JSON-RPC error.

### Cleanup and cancellation

Both actuating tools wrap their body in the same guard: read the cover
state before anything moves (a failed read aborts with nothing to clean
up), close the cover, run the body, then — **on every exit** —
`calibrator_off` and, only if the cover started `Open`, `open_cover`.
`cover_restored` in the result is `true` exactly when the cover started
open and was reopened; a cover that started closed has nothing to
restore.

Cleanup never masks the body's outcome. A `calibrator_off` failure is a
warning; an `open_cover` failure is a warning; and an `open_cover`
**refused by `rp`'s safety gate** (the night turned unsafe while the
flats ran — JSON-RPC error `-32010`) is a warning that says the cover
correctly stays closed — never a failure of the flats. If the unsafe
transition lands *during* an exposure, `rp` aborts that exposure, the
provider's `capture` fails, cleanup runs, and the tool returns `rp`'s
error: the same thing that happens to lights or darks.

A client cancellation (a stopped document, an operator cancel, the
caller's connection dropping) reaches the provider as
`notifications/cancelled` through `rp`'s proxy. The tool body watches
its request token: the in-flight `rp` call is cancelled with its own
`notifications/cancelled` (so `rp` aborts the exposure instead of
finishing it into the void), the body returns `cancelled: <reason>`,
and cleanup runs on a client whose token the cancellation cannot reach —
panel off, cover restored if it started open. The body runs on its own
task, so cleanup completes even if the transport has stopped waiting
for the answer.

### Verification

No verification exposure. Every flat `take_flats` captures is measured
with `compute_image_stats` after the fact; a median outside 50 % of
`max_adu` ± `flat_warn_tolerance` (default 10 %) gets a `warn!` naming
the file and lands in the filter's `out_of_range` list and the result's
`warnings`. The run continues. A statistics call that fails is itself a
warning naming the file. The search's own convergence `tolerance` (5 %)
is a separate knob.

## Exposure search

The proportional search is unchanged from the orchestrator days:

```
new_duration = current_duration * (target_adu / measured_median)
```

with a doubling guard for a zero median, clamped above at the camera's
`exposure_max`. Convergence is a measured deviation within `tolerance`;
`max_iterations` bounds one pass.

**The floor** (plan D8): a search never exposes shorter than the larger
of `min_exposure` (default `250ms`) and the camera's `exposure_min`. The
first exposure of a pass is at least the floor, and a step that *wants*
less than the floor ends the pass at once as **over-bright** — short
flats where shutter timing dominates are avoided by dimming the panel,
not by accepting them.

**The brightness ladder**: a pass that ends over the target — pinned
by saturation with no gradient to descend, or stopped at the floor —
halves the panel brightness (`calibrator_on` at `floor(brightness / 2)`)
and runs again from the last duration, never re-brightens, persists
across filters (the level that worked for one filter is the next one's
start), and stops at a floor of 1. A pass that ends *under* the target
is not retried: dimming further cannot help. Either stop reports the
filter as `unconverged` with the best duration and the last median.

## Store

One redb file, `calibrator-flats.redb`, with the `rp-targets`
conventions ([rp-targets.md](../crates/rp-targets.md)): a `meta` table
carrying `schema_version`, serde-tolerant record values (new fields
default, unknown fields are kept), a refusal to open a file written by a
newer build.

- **Path.** `store_path` when set; otherwise the platform state
  directory — `/var/lib/rusty-photon/calibrator-flats/` on Linux (the
  packaged unit's `StateDirectory=`), `%PROGRAMDATA%\rusty-photon\calibrator-flats\`
  on Windows, `~/Library/Application Support/rusty-photon/calibrator-flats/`
  on macOS (beside the config, as `rp` does) — resolved through
  `rusty-photon-config`. The parent directory is created on open.
- **Key.** Train id plus filter name. A filterless train stores under
  the train id with no filter — no made-up label.
- **Record.** `train_id`, `filter` (or null), `duration`, `brightness`
  (the ladder level the search settled on), `median_adu`, `max_adu`,
  `bin_x`, `bin_y`, `gain`, `offset` (null when the driver has none),
  `camera_id`, `trained_at` (RFC 3339, UTC).
- **Writes.** A converged search overwrites the record for the same
  train and filter; an unconverged search writes nothing. `take_flats`
  and `get_flat_training` never write.
- **Staleness.** A record is stale when any of `camera_id`, `max_adu`,
  `bin_x`, `bin_y`, `gain`, `offset` differs from what `get_camera_info`
  reports now; every changed field is named, as
  `<field> changed from <recorded> to <current>`.

## Configuration

The config file is required (`ConditionPathExists` on the packaged unit):
`mcp_server_url` has no sensible default. Run standalone, `--config`
names it; when omitted the path resolves to the platform default
(`~/.config/rusty-photon/calibrator-flats.json` on Linux,
`%PROGRAMDATA%\rusty-photon\calibrator-flats.json` on Windows) via
`rusty-photon-config`.

```json
{
  "server": {
    "port": 11170,
    "bind_address": "0.0.0.0",
    "tls": null,
    "auth": null
  },
  "mcp_server_url": "https://localhost:11115/mcp",
  "service_auth": { "username": "observatory", "password": "secret" },
  "ca_cert": "/etc/rusty-photon/pki/ca.pem",
  "tolerance": 0.05,
  "max_iterations": 10,
  "initial_duration": "1s",
  "min_exposure": "250ms",
  "flat_warn_tolerance": 0.10,
  "store_path": null
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `server` | object | `{ "port": 11170 }` | The shared `ServerConfig` ([ADR-016](../decisions/016-service-config-ownership-and-doctor.md)): `port`, `bind_address`, optional `tls` / `auth`. Guards `/mcp` and `/health` alike |
| `mcp_server_url` | string | required | `rp`'s MCP endpoint, dialed per tool call |
| `service_auth` | object or null | null | HTTP Basic credential presented to `rp` — the D6 observatory credential; sent only over verified HTTPS ([ADR-017](../decisions/017-standard-mcp-client-construction.md)) |
| `ca_cert` | string or null | null | PEM CA path used to trust a TLS-enabled `rp` |
| `tolerance` | float | 0.05 | Convergence: acceptable deviation of the measured median from the 50 % target |
| `max_iterations` | int | 10 | Search exposures per pass, per brightness level |
| `initial_duration` | humantime | `"1s"` | Where a filter's search starts |
| `min_exposure` | humantime | `"250ms"` | Search floor; the effective floor is the larger of this and the camera's `exposure_min` |
| `flat_warn_tolerance` | float | 0.10 | Verification band around the 50 % target for `take_flats` warnings |
| `store_path` | string or null | null | Override for the redb file (see [Store](#store)) |

`--port` / `--bind-address` override `server.port` / `server.bind_address`
from the command line. Unknown keys fail the load (`deny_unknown_fields`).

**Retired keys fail loud.** `camera_id`, `filter_wheel_id`,
`calibrator_id`, `filters`, `brightness` and `target_adu_fraction` — the
flat plan of the orchestrator era — are refused at load with a message
naming the key and where the plan went (the tool arguments), rather than
being ignored as unknown. There is no flat plan in the config: the train
is a tool argument, the filters come from the wheel, the fraction is
fixed.

`calibrator-flats doctor [--config <file>] [--json]` diagnoses the config
read-only through the same load path — [doctor.md § Per-service
doctors](doctor.md). Doctor `--fix` wires `service_auth` and `ca_cert`
like every other MCP client of `rp`.

### Registration in `rp`

`rp` learns about the provider from a `plugins[]` entry
([rp.md § Tool Provider Registration](rp.md#tool-provider-registration)).
The registration carries the gate opt-out and the dependency list;
without the `gate` map the three tools come up gated:

```json
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
```

The `rp` tools the provider calls are exactly the `requires_tools` list.
A train the tools address must list its cover calibrator first
([rp.md § Optical Trains](rp.md#optical-trains)).

## Module Structure

```
services/calibrator-flats/src/
  main.rs            CLI entry point (clap + ServiceRunner)
  lib.rs             ServerBuilder / BoundServer; the MCP Host allowlist
  config.rs          Config (server, rp client, search tunables, store_path); retired-key refusal
  doctor.rs          `doctor` subcommand
  error.rs           CalibratorFlatsError (thiserror)
  store.rs           FlatStore (redb), FlatRecord, CameraFacts, staleness
  mcp_client.rs      McpClient: rp-mcp-client wrapper; cancellable calls; FlatsRig impl
  workflow.rs        FlatsRig trait; train resolution; exposure search + ladder + floor;
                     train_flats / take_flats / get_flat_training bodies; the cover/panel guard
  tools.rs           rmcp ServerHandler: the three #[tool]s, progress relay, cancellation
  routes.rs          Axum router: GET /health, /mcp
```

## Testing Strategy

Testing follows the conventions in `docs/skills/testing.md`.

### BDD Tests (Cucumber)

`services/calibrator-flats/tests/features/flats_tools.feature` runs the
real three-process topology — OmniSim, calibrator-flats, `rp` with the
provider registered — and calls the tools **through `rp`'s proxy** with
the harness MCP client, exactly as a document would. The provider is
started before `rp` (it must answer `tools/list` on its own) with `rp`'s
port pinned in advance. Scenarios: the tools appear in the catalog
ungated; `train_flats` records a converged filter; an unconverged filter
is reported and writes nothing; `take_flats` refuses an untrained filter
and a stale record (naming the changed field) before actuating; it
captures the requested frames and warns on an out-of-range median; a
cancelled `take_flats` turns the panel off and restores an open cover; a
refused `open_cover` after an unsafe transition is a warning, not a
failure.

`auth.feature` spawns only calibrator-flats with `server.tls` and
`server.auth` and proves `/health` and `/mcp` both require the
credential — and that `tools/list` answers with no `rp` running.
`doctor.feature` is the shared doctor smoke.

### Unit Tests

- Config: defaults, the `server` block, CLI overrides, retired keys
  named, unknown keys rejected, the default store path per platform.
- Store: round trip, overwrite, the filterless key, reopen, a newer
  schema refused, staleness per field.
- Workflow, against a `mockall` rig: the proportional step and its
  clamps, the floor (a start below it is raised, a step below it ends
  the pass over-bright), the ladder, train resolution and filter
  selection, `take_flats`'s pre-flight refusal actuating nothing, the
  cleanup guard on success, error and cancellation, the refused reopen
  as a warning, unconverged-writes-nothing.
- Tools: the result shapes and the error text for the argument-level
  refusals.

## Future Considerations

- **Rotator-aware and per-filter-brightness flats** (plan O2): taking
  flats at the light frames' rotator angle is a `take_flats` argument
  for a later plan; brightness is already per filter in the record.
- **Panel aging** (plan O5): deliberately not modeled. If retraining
  becomes a chore, write-back of a corrected duration fits the record
  shape.
- **Two calibrator devices on one train** (plan O1): a motorized dust
  cap plus a separate light panel needs capability-resolved addressing
  in `rp`'s train model.

# planetarium-bridge

## Overview

`planetarium-bridge` serves a **virtual ASCOM Alpaca Telescope** that
planetarium apps (SkySafari 7+, Stellarium, Cartes du Ciel) connect to
as if it were a mount. Pressing **Align** in the planetarium does not
correct any pointing model — it **imports the selected coordinates as
a paused target** into rp's target store, named by reverse catalog
lookup, for the operator to review and activate later. GoTo is
accepted as simulated motion but never imports: SkySafari refuses
every GoTo to a target below the horizon, which would restrict
planning to currently-risen targets — unacceptable for couch
planning — while it never refuses an Align. The service never touches
hardware and is never on the imaging path; rp's planner images the
target whenever conditions are right, fully decoupled from the
planetarium (workspace tenet 3 is satisfied trivially: there is nothing
to actuate).

Every statement here about what SkySafari does is an observation of
SkySafari Pro 8.0.3 on the wire, on an iPad and on a phone running the
same build. SkySafari 7, the oldest version the bridge is meant for,
is not separately tested, and neither Stellarium nor Cartes du Ciel
is measured. Two of the observations decide the design against what
the ASCOM contract suggests — every GoTo below the horizon is refused
while an Align never is, and a position report below the horizon
wedges GoTo on one device and not on the other — and the sessions
that showed them are recorded in
[planetarium-bridge-skysafari-sessions-2026-07.md](../plans/archive/planetarium-bridge-skysafari-sessions-2026-07.md).

## Architecture

```
 SkySafari / Stellarium / CdC                    planetarium-bridge (port 11126)
┌───────────────────────────┐    Alpaca HTTP    ┌──────────────────────────────────────┐
│  "scope" preset →         │ ────────────────► │ ascom-alpaca server: one Telescope    │
│  connect, 1 Hz poll,      │                   │  ├─ virtual pointing state machine    │
│  Align=import, GoTo=sim   │ ◄──────────────── │  │   (simulated slews, altitude floor)│
└───────────────────────────┘   position reports│  └─ site/LST/alt-az (rp-ephemeris)    │
                                                │                                      │
                                                │ import pipeline                      │
                                                │  Align coords ─ epoch ─► ICRS        │
                                                │        │                             │
                                                │        ▼            rp down?        │
                                                │  rp-mcp-client ◄──── on-disk spool   │
                                                │  (auth, pinned CA)   (bounded FIFO)  │
                                                └───────────┬──────────────────────────┘
                                                            │ MCP add_target{coords, source}
                                                            ▼
                                        rp (port 11115): cone-search naming, dedup,
                                        slug allocation, pending target in the store
```

Component boundaries:

| Component | Home | Role |
|---|---|---|
| Virtual Telescope device | `services/planetarium-bridge` | Full ASCOM contract, simulated motion, reported-position policy |
| Import pipeline + spool | `services/planetarium-bridge` | Epoch conversion, `add_target` submission, offline spooling, `/health` |
| MCP client | [`rp-mcp-client`](../../crates/rp-mcp-client/) | Authed, CA-pinned transport — no bridge-local HTTP code |
| Ephemeris (LST, alt-az) | [`rp-ephemeris`](../crates/rp-ephemeris.md) | Site math for reports and the altitude floor |
| Epoch conversion | `erfars` | Apparent-of-date → ICRS when `assume_epoch = "jnow"` |
| Reverse cone-search + naming + dedup | **rp** (with `rp-catalog`, `rp-targets`) | Everything that needs the store — see [rp-side contract](#rp-side-contract) |

The bridge is deliberately thin: it owns the wire persona and the
delivery guarantee. All naming, dedup, and slug policy live in rp,
where the store is — the bridge sends bare ICRS coordinates plus
provenance and nothing else.

Standard service scaffolding applies
([service-lifecycle.md](../skills/service-lifecycle.md)): `ServiceRunner`
with SCM feature and its in-process reload (`run_with_reload`, which a
[setup-page](#setup-pages) save fires), `init_service_tracing`,
`resolve_and_init` config
bootstrap minting the Alpaca `UniqueID`, and `pkg/doctor.toml`
(`class = "alpaca"`, port 11126).

## The virtual Telescope device

One Alpaca `Telescope` (device number 0) via the `ascom-alpaca` server
feature.

### Identity and capabilities

The device name and description state loudly that this is a **virtual
target-entry device, not a mount** — e.g. name
`"Planetarium Bridge (virtual target entry — NOT a mount)"`. The
`UniqueID` is minted at first start by `resolve_and_init`.

| Member | Value |
|---|---|
| `AlignmentMode` | `GermanPolar` (SkySafari connects to it and slews) |
| `EquatorialSystem` | `J2000` |
| `CanSlew` / `CanSlewAsync` | `true` |
| `CanSync` | **`true`** — sync **is** the import gesture; see [Align is the import gesture](#align-sync-is-the-import-gesture) |
| `CanSyncAltAz`, `CanSlewAltAz`, `CanSlewAltAzAsync` | `false` |
| `CanPark`, `CanUnpark`, `CanFindHome`, `CanSetPark` | `false` (`AtPark` reads `false`) |
| `CanMoveAxis`, `CanPulseGuide`, `CanSetGuideRates` | `false` |
| `CanSetTracking`, `CanSetDeclinationRate`, `CanSetRightAscensionRate` | `false` |
| `Tracking` | reads `true` (constant) |
| `SideOfPier` | `NOT_IMPLEMENTED` (legal for ITelescopeV3; SkySafari never reads it) |
| `DestinationSideOfPier` | hour-angle prediction: positive-HA (west-of-meridian) targets answer `East`, the rest `West` — ConformU requires a GermanPolar device to answer differently on the two sides of the meridian |
| `UTCDate` read | host clock UTC |
| `SetUTCDate` | `NOT_IMPLEMENTED` (SkySafari retries once and carries on) |
| `AxisRates` | empty set |
| `TargetRightAscension` / `TargetDeclination` | session state: cleared on every `Connected = true`, so a fresh connection reads `VALUE_NOT_SET` until it writes them (the ASCOM read-before-write contract) |

### Epoch handling

The device declares `EquatorialSystem = J2000` and SkySafari honors
it (arcsecond-exact J2000 on the wire, read once at connect).
`assume_epoch` config covers clients that ignore the declaration:

- `"j2000"` (default) — received coordinates are ICRS/J2000; used as-is.
- `"jnow"` — received coordinates are apparent-of-date; converted to
  ICRS via ERFA (`Atic13`, with TT derived from host UTC) before import.

The conversion happens once, at receipt — identically for slew and
sync coordinates; everything downstream (spool, rp, the store) is
ICRS only.

### Slew lifecycle — simulated motion, never an import

All three ASCOM slew forms are accepted and treated identically:
`SlewToCoordinatesAsync` (the only one SkySafari sends),
`SlewToCoordinates` (blocking — completes after the convergence
window), and `SlewToTarget`/`SlewToTargetAsync` (via the
`TargetRightAscension`/`TargetDeclination` setters, which validate and
store per the ASCOM contract; slew verbs propagate the requested
coordinates into `Target*` as ConformU expects).

On any slew verb:

1. Validate ranges (`ra ∈ [0,24)`, `dec ∈ [-90,90]`) — out-of-range →
   ASCOM `InvalidValue`.
2. Start the simulated slew: `Slewing` reads `true` for the
   convergence window (`slew_duration`, default `3s`, which
   SkySafari's 1 Hz poll shows as a slew that arrives), with reported
   position interpolating
   from the current pointing to the target (shortest-path RA wrap).
   A new slew verb during convergence supersedes: the new slew starts
   from the current interpolated position. `AbortSlew` ends the
   simulated motion (always callable, stop-class).

Slews never create targets. GoTo cannot be the gesture: SkySafari
refuses **every** GoTo form — object *and* coordinate-entry — for a
below-horizon target, under every horizon display setting, and sends
nothing, so a GoTo-based import would restrict planning to
currently-risen objects, defeating the couch-planning use case; and
the [wedge](#reported-position-policy-the-altitude-floor) makes GoTo
availability depend on client state besides. A GoTo is still
useful to the operator (the scope marker moves, confirming the
connection is live), but it expresses "point there", not "keep this".

### Reported-position policy (the altitude floor)

SkySafari can **refuse every GoTo while the scope's reported position
is below the horizon** — a below-horizon report wedges the whole
session. This can happen with no operator error at all: a tracked
position set at dusk sinks below the horizon hours later. The wedge
is not reliable: of two devices running the identical SkySafari
build, one wedges and the other does not, under any horizon display
setting, so the hazard depends on client state and cannot be
predicted — the floor is cheap, always-on defense. Because Align is
the import gesture a wedge cannot block imports, only GoTo's
confirm-the-connection usefulness. The bridge maintains two notions
of position:

- **Virtual pointing** — where the last slew converged. Follows the
  slew interpolation, then holds (RA/Dec constant, i.e. tracking).
- **Reported position** — what `RightAscension`/`Declination` return:
  the virtual pointing *while its computed altitude ≥
  `report_altitude_floor_deg`* (default `10.0`), otherwise the **idle
  point**: RA = current LST, Dec = site latitude (the zenith).

The floor is evaluated at read time from the live site. The idle point
self-heals: parked at the zenith, the (tracking-constant) RA drifts
west of the meridian over the hours; when its altitude eventually
reaches the floor, the reported position re-snaps to the then-current
zenith. The operator-visible effect: the scope marker sits on the last
imported target while that target is meaningfully up, and drifts to
"parked overhead" when it sets — and the client can always GoTo.

`report_altitude_floor_deg: null` disables the policy (reports raw
virtual pointing) — safe on a client known not to wedge, but the
default is `10.0` because one build wedges on one device and not on
another.

### Align (sync) is the import gesture

`CanSync = true`; `SyncToCoordinates` (the only form SkySafari sends)
and `SyncToTarget` are both accepted. On any sync verb:

1. Validate ranges — out-of-range → ASCOM `InvalidValue`, no import.
2. Convert per [`assume_epoch`](#epoch-handling) and **fire the
   import** (§ [import pipeline](#the-import-pipeline)) — the Align
   tap is the operator's intent, so the import happens at receipt.
3. Set the virtual pointing to the synced coordinates (the ASCOM
   contract: the scope "is" now there). The reported position stays
   subject to the
   [altitude floor](#reported-position-policy-the-altitude-floor), so
   a below-horizon Align parks the *report* at the idle point while
   the import proceeds normally.

The operator workflow is **Center → Align**: Center frames the object
on screen (display-only: it sends nothing), Align imports it.
Repeated Aligns on the same object collapse via rp-side proximity
dedup (three Aligns on NGC 253 are one pending target).

ASCOM's sync corrects a pointing model, and importing on it bends
that meaning knowingly, for a virtual device that has no model to
correct. Align is the **only** gesture that works regardless of
horizon — SkySafari refuses both GoTo forms for below-horizon
targets, and planning must not be restricted to currently-risen
objects. Neither objection to importing on a sync holds: a sync that
puts the pointing below the horizon cannot wedge the client, because
the altitude floor keeps the report above it, and a casual Align tap
costs one paused inbox row that dedup or a discard tap cleans up.

Limitation: SkySafari offers Align only on a **selected object**,
never on an entered coordinate point — so a direct arbitrary-point
import does not exist under the Align gesture. The framing path — at
any altitude — is the faint-star anchor: select a faint HD/Tycho
star at the intended frame center and Align on it; the rp-side star
naming tier (2′ cone) then names the import after exactly that
anchor. SkySafari's selectable catalog is deep enough that an anchor
within arcminutes of any frame center almost always exists.

### Site and time writes

SkySafari pushes its own GPS-derived site
(`PUT SiteLatitude`/`SiteLongitude`) and `PUT UTCDate` after connect.

- **Site writes are adopted live**: the pushed values replace the
  configured site for LST/alt-az/floor math (and are reflected on
  reads). The client's site is typically *more* accurate than a static
  config, and using it keeps the bridge's horizon math consistent with
  the client's own — exactly what the floor policy wants. Adoption is
  logged at `info!`; the configured site is the startup default, and a
  restart, or the reload a [setup-page](#setup-pages) save fires, reverts
  to it. Latitude/longitude writes are range-checked
  (`InvalidValue` outside them); `SiteElevation` writes accept the
  ASCOM range `[-300, 10000]` meters.
- **`SetUTCDate` stays `NOT_IMPLEMENTED`** — the host clock is
  authoritative for a virtual device, and SkySafari tolerates the
  rejection (one retry, no fallout).

### Discovery

Alpaca UDP discovery follows the fleet convention: **opt-in, off by
default** (`server.discovery_port` absent), because many rusty-photon
Alpaca servers on one host would collide on the shared port — the
`ports.discovery-collision` doctor check guards this. The documented
connection story is manual IP:port entry, which is also the only one
that works across routed subnets (discovery is broadcast-scoped).

### ConformU

The device passes ConformU (`bazel test --config=conformu`). The
capability matrix above is deliberately minimal-but-coherent: every
`false` capability's verbs return `NOT_IMPLEMENTED`, every `true`
capability behaves per the ASCOM contract (Target* propagation,
`Slewing` state, `AbortSlew` always callable as a stop-class verb).
Two `CanSync = true` consequences: sync verbs must round-trip per the
ASCOM contract, and the altitude floor can mask that round-trip (a
below-floor sync reads back as the idle point) — the conformance run
therefore uses a config with `report_altitude_floor_deg: null` (the
floor is a client-UX policy, not device semantics). ConformU's sync
tests also fire imports; the harness points `rp.mcp_server_url` at a
dead loopback port so they land in the scenario's spool.

Three harness specifics, all consequences of ConformU's own behavior
rather than device semantics:

- The suites run via ConformU's `*-settings` commands (the device
  under test is configured inside the settings file;
  `bdd_infra::run_conformu_from_settings`). The URL-argument commands
  force the full Telescope test set (`SetFullTest()` rebuilds the
  `TelescopeTests` dictionary all-enabled), and a
  `CanPulseGuide = false` device cannot satisfy the full set: the
  protocol suite's PulseGuide test polls `IsPulseGuiding` as its
  completion check and records the spec-mandated `NOT_IMPLEMENTED`
  answer as an error, while the conformance suite requires exactly
  that answer. The settings deselect the PulseGuide test
  (`TelescopeTests`), which only the `*-settings` commands honor.
  That is the only deselection: every other Telescope selection flag
  is written out at the value `SetFullTest()` forces, because
  ConformU's own defaults leave the two side-of-pier flags
  (`TestSideOfPierRead`, `TestSideOfPierWrite`) off. With them on,
  ConformU still runs neither of the tests they select: the
  side-of-pier model tests, and the `SideOfPier` write test nested
  inside them, run only when `SideOfPier` can be read, and this device
  answers `NOT_IMPLEMENTED`, so ConformU logs an `INFO` skip.
- Deliberately deselected tests surface as ConformU "configuration
  alerts", which count into its exit code like errors. The runner
  therefore takes the conformance verdict from ConformU's results file
  and requires zero errors, zero issues and exactly the alerts the test
  names (`EXPECTED_ALERTS`): here the single "Pulse guide tests were
  omitted due to Conform configuration", so a clean conformance run
  ends `0 issues, 0 errors and 1 configuration alert`. The protocol
  suite reports the omission as an information message, which counts
  neither as an error nor as an issue; it must exit zero and print a
  summary of `0 errors, 0 issues`.
- The harness config sets `slew_duration: "5s"`: ConformU's AbortSlew
  test validates `Slewing == true` a fixed 1.5 s after starting an
  async slew, so the convergence window must comfortably exceed that.

## The import pipeline

### Align → `add_target`

Each accepted sync verb produces one import request:

```jsonc
// MCP tool call to rp
add_target {
  "ra_hours": 20.9877,          // ICRS, post-assume_epoch conversion
  "dec_degrees": 44.5253,
  "source": {
    "kind": "planetarium-bridge",
    "client": "<ip:port of the planetarium>",
    "received_at": "2026-07-29T05:41:12.481Z"
  }
}
```

The bridge sends **no name** — naming, dedup, slug allocation, and
default goals are rp policy (§ [rp-side contract](#rp-side-contract)).
On success the bridge logs the outcome at `info!` (the one log line an
operator derives clear value from):
`imported as ngc7000-2 (created)` / `updated pending target ngc7000-2`.

Tool failures (rp rejected the call — e.g. a validation error) are
logged at `error!` and **not** spooled: a request rp actively rejected
will be rejected again on replay. Only *delivery* failures spool.

The bridge holds **no standing MCP client**: it connects for a
delivery burst and drops the client when the queue goes idle. An idle
held client keeps a long-lived stream open into rp, which stalls rp's
own graceful shutdown: rp cannot be stopped while an idle bridge sits
connected. The transport is session-less, so nothing else is lost by
dropping the client; imports are human-paced and the extra
`server/discover` round-trips are noise-level.

### Spooling — rp unreachable

rp being down must never lose an Align. Delivery failures (transport
loss, TLS failure, timeouts) append the import request to a **bounded
on-disk FIFO spool**:

- One JSONL file (`spool.path`, default
  `<platform config root>/planetarium-bridge/spool.jsonl` beside the
  service's other state), one request per line, `fsync`ed per append —
  a Ctrl-C or crash loses nothing, and the spool **replays across
  bridge restarts** and the in-process reload a
  [setup-page](#setup-pages) save fires.
- Replay runs in order (FIFO) whenever rp is reachable again, paced by
  exponential backoff between reconnect attempts (1 s doubling to
  `spool.replay_backoff_max`, default `5m`). Replayed entries carry
  their original `received_at`, so provenance reflects the Align, not
  the replay.
- **Bounded**: at `spool.max_entries` (default `1000`, comfortably
  above any human session), the oldest entry is dropped to admit the
  newest — with an `error!` log *per drop* and the `dropped_total`
  counter incremented. "Never drop silently" means observable, not
  infallible.

Replay is idempotent by construction: a replayed request hits the same
rp-side dedup as a live one, so the worst case of a
crash-between-send-and-remove is an in-place upsert of the same
pending target.

### `/health`

The bridge serves `GET /health` alongside the Alpaca routes (same
listener), returning:

```json
{ "rp_reachable": true, "spooled": 0, "replayed_total": 3, "dropped_total": 0 }
```

`spooled` is the durable backlog length; the totals are
process-lifetime counters, which a [setup-page](#setup-pages) reload
keeps. The endpoint is for an operator's `curl`: sentinel probes
alpaca-class services at the Alpaca `connected` endpoint only and
does not read it.

## rp-side contract

Everything past delivery is rp's. The bridge sends bare ICRS
coordinates and a `source`; rp's `add_target` names the target,
de-duplicates it, allocates its slug and stores it. That contract is
specified where it is implemented — [rp.md § Target Store](rp.md#target-store),
under [Writer identity](rp.md#writer-identity) and
[Import form](rp.md#import-form-source), with the catalog query behind
the naming in
[rp-catalog.md](../crates/rp-catalog.md#nearest-neighbor-naming). What
the bridge's own behavior leans on:

- **An import lands paused** (`active: false`), with rp's name, slug
  and default goals. The bridge supplies none of them, and rp rejects
  a name sent alongside `source`.
- **Dedup is coordinate proximity** (`target_store.import.dedup_arcsec`,
  default 30″, below any mosaic panel spacing). A repeat Align on the
  same object updates the pending import in place while the operator
  has not touched it; a row that is active, operator-edited or
  operator-created is never modified, and the import lands beside it
  under a suffixed slug. This is what makes a casual second tap
  harmless and [spool replay](#spooling--rp-unreachable) idempotent,
  and it is enforced in rp, not left to the bridge's courtesy.
- **Naming is a reverse cone-search**: a deep-sky object within 10′
  names the import, else a star within 2′, else the coordinate form
  (`"J2059+4432"`). The star tier is what names the
  [faint-star anchor](#align-sync-is-the-import-gesture) after the
  star the operator tapped. Catalog coverage bounds the quality of the
  name only: a target the catalog has never heard of imports exactly
  as well as M31, under its coordinates, ready for a rename during the
  paused review.
- **Writer identity** separates the bridge's rows from the
  operator's: rp stamps `created_by` / `updated_by` with `source.kind`
  (`"planetarium-bridge"`), and any operator write stamps
  `"operator"`, which is how rp knows an import is still unedited.

## Configuration (bridge)

Follows the fleet conventions: durations are humantime strings, angles
bare decimal degrees, `AlpacaServerConfig` for the server block,
sentinel's `service_auth`/`ca_cert` field shape for the client wiring,
`resolve_and_init` minting `device.unique_id` on first start. The `device` section is optional, so a file that leaves it out
gains it from the defaults along with the minted id. The bootstrap writes
the file only when the result loads, and a file the bridge would refuse
is left byte for byte as it was (see
[docs/crates/rusty-photon-config.md](../crates/rusty-photon-config.md)).

```jsonc
{
  "server": {                        // rusty-photon-server-config AlpacaServerConfig
    "port": 11126,
    "bind_address": "0.0.0.0",
    // "discovery_port": 32227,      // opt-in; absent = no discovery responder
    "tls": null,
    "auth": null
  },
  "site": {                          // startup default; a client site push overrides live
    "site_latitude_deg": 33.0,       // WGS84, +N
    "site_longitude_deg": -117.0,    // WGS84, +E (ASCOM convention)
    "site_elevation_m": 0.0
  },
  "rp": {
    "mcp_server_url": "https://rp.example.com:11115/mcp",
    "service_auth": { "username": "observatory", "password": "<plaintext>" },
    "ca_cert": "/etc/rusty-photon/pki/ca.crt"
  },
  "device": {
    "slew_duration": "3s",                  // simulated convergence window
    "assume_epoch": "j2000",                // or "jnow" for clients ignoring the declaration
    "report_altitude_floor_deg": 10.0      // null disables the reported-position floor
  },
  "spool": {
    "path": null,                           // null = platform default location
    "max_entries": 1000,
    "replay_backoff_max": "5m"
  }
}
```

Config invariants follow parse-don't-validate
([development-workflow.md](../skills/development-workflow.md#parse-dont-validate-for-config)):
latitude/floor ranges, positive `max_entries`, a well-formed
`mcp_server_url` — all rejected at load with the field named.

## Setup pages

The bridge configures itself on the two HTML pages the Alpaca Management
API defines, served on its own listener behind its own TLS and Basic Auth:
`/setup` for the whole server and `/setup/v1/telescope/0/setup` for the
virtual Telescope. The pages are the shared
[`rusty-photon-setup`](../crates/rusty-photon-setup.md) crate's — the
render, the save, the cross-site check, the Security section and the
reload wait — and `setup.rs` supplies the bridge's half
(`ConfigurableDriver for PlanetariumBridgeDriver`): its `Config`, the
rules a save adds, its two secrets, and which section the Telescope's page
edits. The binary has no command-line overrides beyond `--config` /
`--log-level`, so `Overrides = ()` and no field is ever pinned. Its
`SupportedActions` is empty.

| Page | Section | Fields |
|------|---------|--------|
| `/setup` | `rp` | `mcp_server_url`, `ca_cert`; `service_auth` is named as edited in the configuration file — an optional block the form does not render, which doctor's client wiring writes |
| | `server` | `port`, `bind_address`, `discovery_port`; `tls` is named as file-only (doctor writes it); `auth` is the Security section |
| | `site` | `site_latitude_deg`, `site_longitude_deg`, `site_elevation_m` — the startup default a client's site push overrides |
| | `spool` | `path` (cleared, the platform default location), `max_entries`, `replay_backoff_max` |
| `/setup/v1/telescope/0/setup` | `device` | `slew_duration`, `report_altitude_floor_deg` — cleared, it saves the `null` that disables the floor — `assume_epoch` as a dropdown of its two words (`j2000`, `jnow`), and `unique_id` under the Identity disclosure |

- **Secrets:** `/server/auth/password_hash`, edited through the Security
  section and never shown, and `/rp/service_auth/password`, the plaintext
  password the bridge presents to rp, which no page carries.
- **rp's credential is bound to rp's address and CA.** The bridge presents
  the credential to whatever server the file names, and a page anyone who
  reaches the bridge can save must not send rp's password to a server typed
  on it. A save that changes `rp.mcp_server_url` or `rp.ca_cert` therefore
  asks first (`409`). Behind the bridge's own `server.auth`, typing rp's
  password there keeps `rp.service_auth` for the new address; each check
  costs an Argon2id hash, as a guess against rp's own login does. A bridge
  without `server.auth` offers no password box, so a guess there needs a
  login set first, which the operator would meet on their next visit. Left blank,
  or without the box, the save removes the credential from the file, and the
  saved page says so, naming the field that changed, and stays on screen
  through the reload rather than reopen the setup page. Without the
  credential, every import gets rp's `401` and is spooled. A removed
  credential is set again in the file, or by `doctor --fix`, which writes an
  absent client credential block ([doctor.md](doctor.md) § What `--fix`
  adds). The page does this with the crate's `with_bound_credential`
  ([rusty-photon-setup.md](../crates/rusty-photon-setup.md) § Credentials).
- **Validation.** The ranges, the URL's shape and the spool bound are the
  config newtypes', checked when the submission is parsed: a value out of
  range is refused (`422`) with the newtype's message, which names the
  field, above the form, and the file is left as it was. A save adds two
  rules: `device.unique_id` must be non-empty — the bootstrap mints an id
  only when the process starts, so a reload would serve a cleared one as
  it is — and `spool.replay_backoff_max` must be greater than zero, since
  the replay backoff doubles from one second up to it and a zero cap would
  retry a down rp in a tight loop. These two are the page's: the load does
  not apply them, so a file already holding a zero cap starts as before,
  and every save is refused, naming the field, until it is fixed.

A save that changes a field persists atomically, answers that the service
is reloading, and fires the in-process reload after the response has
flushed — a reload rather than a restart (`SaveDisposition::Reload`):
`main.rs` runs under `ServiceRunner::with_reload().run_with_reload(...)`,
whose loop (`run_server_loop`) rebuilds the server from the
freshly-persisted file. The spool is what makes the stop matter. A reload
stops the serve loop, draining in-flight requests and closing keep-alive
connections, then the import worker: it stops after the delivery in
flight, never in one, spooling any Align still queued, and is waited for —
aborted past 10 s, so a wedged rp cannot hold the stop — before the next
server loads the spool file, so two workers never rewrite one spool from
their own copies. An abort drops the import it cuts off mid-delivery and
any Align queued behind it unspooled, as a process exit at that point
does; an entry already in the spool stays there. A reload therefore keeps what a restart keeps, and resets what a
restart resets: the client's connection (it reconnects), the session's
`Target*`, and a client-pushed site, back to the configured one until the
client pushes again. `/health`'s totals span it: one `HealthState` outlives
the reloads, with `rp_reachable` false until the new worker's startup
probe settles it. A save that changes nothing says so and fires no reload.

## Doctor integration

- `pkg/doctor.toml`: `class = "alpaca"`, `port = 11126`, listed in
  doctor's embedded catalog (`services/doctor/src/catalog.rs`) — the
  manifest alone does not register a service. Sentinel's health
  supervision and doctor's port checks apply as to any Alpaca service.
- **Client wiring**: doctor's `plan_client_wiring` provisions
  `rp.service_auth` + `rp.ca_cert` (absent-only), exactly as for
  sentinel and session-runner — via nested pointers, since
  the bridge's client block lives under its `rp` key.
- **The fake-mount hazard** (`joins.fake-mount`, doctor.md § The
  fake-mount hazard): doctor **fails when rp's `equipment.mount`
  points at the bridge** — statically when the URL loopback-joins to
  the bridge's port, and by probing the configured mount's management
  API for the bridge's `device.unique_id` otherwise (the leg that
  catches host-name rig URLs). Wiring the virtual device in as rp's
  real mount would defeat every motion safeguard rp believes it has
  (slews that "just succeed", a mount that is never parked, never at
  limits). The check is a hard failure, not a warning.

## Error handling summary

| Condition | Behavior |
|---|---|
| Slew or sync coords out of range | ASCOM `InvalidValue`; no import |
| Slew verbs | Simulated motion only — never an import |
| Motion verbs for `false` capabilities | ASCOM `NOT_IMPLEMENTED` |
| rp rejects `add_target` (tool error) | `error!` log; **not** spooled (would fail again) |
| rp unreachable | Spool append (`fsync` per entry); the sync verb still succeeds normally |
| Spool full | Drop oldest; `error!` per drop; `dropped_total`++ |
| Spool file unreadable at startup | `error!`, start with an empty spool (never refuse to start) |
| Corrupt spool line on replay | Skip + `error!` with the line number; continue |
| Client disconnect | Nothing arrives: SkySafari stops polling and sends no `Connected = false` — no state depends on a disconnect signal |

## Out of scope

- Sentinel scraping `/health`: nothing but an operator reads it.
- `position_angle_degrees` on imports. The field
  ([rp.md § Position angle](rp.md#position-angle)) is **permanently
  operator-owned**: rp rejects it alongside `source`, imports always
  land inheriting the train default, and the [ui-htmx targets
  inbox](ui-htmx.md#targets-inbox-targets) is where per-target angles
  are entered — SkySafari exports its FOV angle through no channel.
- Anything specific to Stellarium or Cartes du Ciel. Both connect to
  this device as to any Alpaca telescope; neither is measured the way
  SkySafari is.
- Renaming earlier imports when a second framing of the same object
  arrives.
- Any per-client identity beyond the provenance stamp: SkySafari's
  `ClientID` differs between app contexts.

## Testing

BDD drives the device with the `ascom-alpaca` **client** feature (the
same harness pattern the other drivers use) plus a **stub rp MCP
server**; rp-side semantics are covered in rp's own BDD suite against
the real store.

| Feature file (bridge) | Scenarios |
|---|---|
| `device_contract.feature` | Capability matrix; sync sets pointing + Target*; slew Target* propagation; abort ends motion; site push adoption; UTCDate write rejected |
| `target_import.feature` | Align → `add_target` (both sync verbs); slews never import; epoch conversion under `assume_epoch: jnow`; below-horizon Align imported; repeated Aligns each submitted (rp dedup collapses) |
| `position_policy.feature` | Floor snap to idle point; below-floor slew converges but reports idle; below-floor Align imports but reports idle; `null` floor reports raw pointing |
| `spooling.feature` | rp down → spool; replay in order on recovery; replay after restart; replay across a setup-page reload; the health totals span a reload; overflow drops oldest with counter; corrupt line skipped; tool-error not spooled |
| `setup_pages.feature` | The [setup pages](#setup-pages): the server page's sections and its link to the Telescope's page; the Telescope's page shows the device section alone, `assume_epoch` a dropdown; a valid save persists, reloads and is served back; clearing the floor saves `null`; an out-of-range value refused naming the field; the zero backoff cap and the empty `unique_id` refused beside their fields; a save that changes nothing; a cross-site `POST` refused; a port change answers with the new address; the rp password never on a page; a save moving rp's address or CA asks, then removes the rp credential with the saved page staying on the removal, rp's password typed on the confirmation behind TLS and auth keeps it, a save keeping both keeps it; with TLS and auth, `401` without the credential and no hash on the page |
| `doctor.feature` | The shared doctor smoke (testing.md § Doctor smoke): a valid config yields a clean report, an unknown key fails the report and is named. Rides `bdd_infra::doctor_smoke`; the world's `valid_config()` spells out all five config blocks, so the clean-report scenario covers the whole typed load path |
| `auth.feature` | The shared TLS + auth smoke (testing.md § TLS/auth): with `server.tls` and `server.auth` configured the bridge serves HTTPS and answers 401 without credentials, 200 with them. Rides `bdd_infra::tls_auth`; the world's launch hook points `spool.path` at the scenario temp dir, since an absent path resolves to — and creates — the platform config dir |

rp's half of the [contract](#rp-side-contract) is covered where it
lives: rp's `target_store_import.feature` for the import form, and
`rp-catalog`'s unit tests for `nearest`.

ConformU runs under `bazel test --config=conformu` against the mock
backend, as for the other drivers.

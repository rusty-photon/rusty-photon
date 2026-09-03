# planetarium-bridge

**Status: implemented in full — bridge service, the [rp-side
contract](#rp-side-contract) (rp.md § Target Store → Import form is
the landed rp-side text), and the [doctor
integration](#doctor-integration) including the fake-mount check.**
This document is the design for the `planetarium-bridge` service —
P3 of
[planetarium-target-import.md](../plans/planetarium-target-import.md).
The P3a verification-spike findings that ground it are preserved in the
[appendix](#appendix-p3a-verification-spike-findings-2026-07-29); the
[P3b follow-up experiment](#appendix-p3b-horizon-experiment-findings-2026-07-30)
(run 2026-07-30) overturned the below-horizon story and **flipped the
import gesture from GoTo to Align** (Decision 2's second amendment).
The [Testing](#testing) section's six feature files run live in the
default suite (69 scenarios), the device passes ConformU (see
[ConformU](#conformu) for the harness specifics), and the throwaway
P3a spike crate is deleted. What this service sends rp is final; the
rp-side `source` semantics landed in rp as their own P3 slice
(authoritative text: rp.md § Target Store → Writer identity / Import
form, exercised by rp's `target_store_import.feature`).

## Overview

`planetarium-bridge` serves a **virtual ASCOM Alpaca Telescope** that
planetarium apps (SkySafari 7+, Stellarium, Cartes du Ciel) connect to
as if it were a mount. Pressing **Align** in the planetarium does not
correct any pointing model — it **imports the selected coordinates as
a paused target** into rp's target store, named by reverse catalog
lookup, for the operator to review and activate later. GoTo is
accepted as simulated motion but never imports: P3b proved SkySafari
horizon-gates every GoTo form unconditionally, which would restrict
planning to currently-risen targets — unacceptable for couch
planning — while Align is never gated. The service never touches
hardware and is never on the imaging path; rp's planner images the
target whenever conditions are right, fully decoupled from the
planetarium (workspace tenet 3 is satisfied trivially: there is nothing
to actuate).

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
                                                │  (ADR-017 auth/TLS)  (bounded FIFO)  │
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
| MCP client | [`rp-mcp-client`](../decisions/017-standard-mcp-client-construction.md) | Authed, CA-pinned transport — no bridge-local HTTP code |
| Ephemeris (LST, alt-az) | [`rp-ephemeris`](../crates/rp-ephemeris.md) | Site math for reports and the altitude floor |
| Epoch conversion | `erfars` | Apparent-of-date → ICRS when `assume_epoch = "jnow"` |
| Reverse cone-search + naming + dedup | **rp** (with `rp-catalog`, `rp-targets`) | Everything that needs the store — see [rp-side contract](#rp-side-contract) |

The bridge is deliberately thin: it owns the wire persona and the
delivery guarantee. All naming, dedup, and slug policy live in rp,
where the store is — the bridge sends bare ICRS coordinates plus
provenance and nothing else.

Standard service scaffolding applies
([service-lifecycle.md](../skills/service-lifecycle.md)): `ServiceRunner`
with SCM feature, `init_service_tracing`, `resolve_and_init` config
bootstrap minting the Alpaca `UniqueID`, `pkg/doctor.toml`
(`class = "alpaca"`, port 11126), workspace/Bazel registration, and the
hand-typed port-table updates (workspace.md, packaging docs, doctor.md)
in the implementation PR.

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
| `AlignmentMode` | `GermanPolar` (spike-proven with SkySafari) |
| `EquatorialSystem` | `J2000` |
| `CanSlew` / `CanSlewAsync` | `true` |
| `CanSync` | **`true`** — sync **is** the import gesture; see [Align is the import gesture](#align-sync-is-the-import-gesture) |
| `CanSyncAltAz`, `CanSlewAltAz`, `CanSlewAltAzAsync` | `false` |
| `CanPark`, `CanUnpark`, `CanFindHome`, `CanSetPark` | `false` (`AtPark` reads `false`) |
| `CanMoveAxis`, `CanPulseGuide`, `CanSetGuideRates` | `false` |
| `CanSetTracking`, `CanSetDeclinationRate`, `CanSetRightAscensionRate` | `false` |
| `Tracking` | reads `true` (constant) |
| `SideOfPier` | `NOT_IMPLEMENTED` (legal for ITelescopeV3; nothing polls it — P3a) |
| `DestinationSideOfPier` | hour-angle prediction: positive-HA (west-of-meridian) targets answer `East`, the rest `West` — ConformU requires a GermanPolar device to answer differently on the two sides of the meridian |
| `UTCDate` read | host clock UTC |
| `SetUTCDate` | `NOT_IMPLEMENTED` (P3a: SkySafari retries once, carries on) |
| `AxisRates` | empty set |
| `TargetRightAscension` / `TargetDeclination` | session state: cleared on every `Connected = true`, so a fresh connection reads `VALUE_NOT_SET` until it writes them (the ASCOM read-before-write contract) |

### Epoch handling

The device declares `EquatorialSystem = J2000` and P3a confirmed
SkySafari honors it (arcsecond-exact J2000 on the wire, read once at
connect). `assume_epoch` config covers clients that ignore the
declaration:

- `"j2000"` (default) — received coordinates are ICRS/J2000; used as-is.
- `"jnow"` — received coordinates are apparent-of-date; converted to
  ICRS via ERFA (`Atic13`, with TT derived from host UTC) before import.

The conversion happens once, at receipt — identically for slew and
sync coordinates; everything downstream (spool, rp, the store) is
ICRS only.

### Slew lifecycle — simulated motion, never an import

*(Amended 2026-07-30 after P3b, settled interactively: slew verbs no
longer fire imports; Align does. See
[Align is the import gesture](#align-sync-is-the-import-gesture).)*

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
   convergence window (`slew_duration`, default `3s` — the cadence P3a
   proved SkySafari accepts), with reported position interpolating
   from the current pointing to the target (shortest-path RA wrap).
   A new slew verb during convergence supersedes: the new slew starts
   from the current interpolated position. `AbortSlew` ends the
   simulated motion (always callable, stop-class).

Slews never create targets. P3b killed GoTo as the gesture: SkySafari
refuses **every** GoTo form — object *and* coordinate-entry — for a
below-horizon target, under every horizon display setting, so a
GoTo-based import would restrict planning to currently-risen objects,
defeating the couch-planning use case; and the P3a wedge additionally
makes GoTo availability client-state-dependent. A GoTo is still
useful to the operator (the scope marker moves, confirming the
connection is live), but it expresses "point there", not "keep this".

### Reported-position policy (the altitude floor)

P3a found that SkySafari **refuses every GoTo while the scope's
reported position is below the horizon** — a below-horizon report
wedges the whole session. This can happen with no operator error at
all: a tracked position set at dusk sinks below the horizon hours
later. P3b could **not** reproduce the wedge on a second device
running the identical SkySafari build (no wedge under any horizon
display setting), so the hazard is client-state-dependent and
unpredictable — the floor stays as cheap, always-on defense. With
Align as the import gesture the wedge can no longer block imports,
only GoTo's confirm-the-connection usefulness. The bridge maintains
two notions of position:

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
virtual pointing) — safe on clients proven wedge-free (the P3b phone
never wedged), but the default stays `10.0` because an identical
build did wedge in P3a.

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
on screen (display-only — P3a/P3b confirmed it sends nothing), Align
imports it. Repeated Aligns on the same object collapse via rp-side
proximity dedup (P3b aligned NGC 253 three times; that is one pending
target).

*(This is Decision 2's second amendment — settled interactively
2026-07-30 after P3b, reversing the 2026-07-29 sync rejection. Why:
Align is the **only** gesture that works regardless of horizon —
SkySafari unconditionally refuses both GoTo forms for below-horizon
targets, and planning must not be restricted to currently-risen
objects. The two rationales for rejecting sync both fell: the
sync-induced wedge is neutralized by the altitude floor (and did not
even reproduce in P3b), and a casual Align tap now costs one paused
inbox row that dedup or a discard tap cleans up. The "sync corrects a
pointing model" semantics are knowingly bent for a virtual device
that has none.)*

Limitation, from P3b: SkySafari offers Align only on a **selected
object**, never on an entered coordinate point — so a direct
arbitrary-point import does not exist under the Align gesture. The
framing path — at any altitude — is the P3a faint-star-adjacent
gesture: select a faint HD/Tycho star at the intended frame center
and Align on it; the rp-side star naming tier (2′ cone) then names
the import after exactly that anchor. SkySafari's selectable catalog
is deep enough that an anchor within arcminutes of any frame center
almost always exists (P3a).

### Site and time writes

P3a: SkySafari pushes its own GPS-derived site
(`PUT SiteLatitude`/`SiteLongitude`) and `PUT UTCDate` after connect.

- **Site writes are adopted live**: the pushed values replace the
  configured site for LST/alt-az/floor math (and are reflected on
  reads). The client's site is typically *more* accurate than a static
  config, and using it keeps the bridge's horizon math consistent with
  the client's own — exactly what the floor policy wants. Adoption is
  logged at `info!`; the configured site is the startup default, and a
  restart reverts to it. Latitude/longitude writes are range-checked
  (`InvalidValue` outside them); `SiteElevation` writes accept the
  ASCOM range `[-300, 10000]` meters.
- **`SetUTCDate` stays `NOT_IMPLEMENTED`** — the host clock is
  authoritative for a virtual device, and P3a proved the rejection is
  tolerated (one retry, no fallout).

### Discovery

Alpaca UDP discovery follows the fleet convention (plan Decision 1):
**opt-in, off by default** (`server.discovery_port` absent), because
many rusty-photon Alpaca servers on one host would collide on the
shared port — the `ports.discovery-collision` doctor check guards
this. The documented connection story is manual IP:port entry, which
P3a confirmed is also the only story that works across routed subnets
(discovery is broadcast-scoped).

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
  force-enable every test, and a `CanPulseGuide = false` device cannot
  satisfy the full set: the protocol suite's PulseGuide test polls
  `IsPulseGuiding` as its completion check and records the
  spec-mandated `NOT_IMPLEMENTED` answer as an error, while the
  conformance suite requires exactly that answer. The settings
  deselect the PulseGuide test (`TelescopeTests`), which only the
  `*-settings` commands honor.
- Deliberately deselected tests surface as ConformU "configuration
  alerts", which count into its exit code like errors; the runner
  accepts a run whose summary shows zero errors and zero issues.
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
own graceful shutdown (found by the MSI lifecycle verify: rp could not
be stopped while an idle bridge sat connected). The transport is
session-less (ADR-021), so that is the only reason left; imports are
human-paced and the extra `server/discover` round-trips are
noise-level.

### Spooling — rp unreachable

rp being down must never lose an Align. Delivery failures (transport
loss, TLS failure, timeouts) append the import request to a **bounded
on-disk FIFO spool**:

- One JSONL file (`spool.path`, default
  `<platform config root>/planetarium-bridge/spool.jsonl` beside the
  service's other state), one request per line, `fsync`ed per append —
  a Ctrl-C or crash loses nothing, and the spool **replays across
  bridge restarts**.
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
process-lifetime counters. This is the plan's "sentinel-visible
counter" hook: curl-able immediately; teaching sentinel's supervisor to
scrape it is a documented follow-up, not MVP (sentinel today probes
alpaca-class services at the Alpaca `connected` endpoint only).

## rp-side contract

**Landed.** Everything in this section lives in `rp` / `rp-targets` /
`rp-catalog` (not the bridge); it activated the `source` parameter
`rp.md` had reserved on `add_target`. The absorbed, authoritative text
is `rp.md` § Target Store (→ Writer identity, → Import form) and
`rp-targets.md`; this section stays as the design record with its
rationale.

### Writer identity: `created_by` / `updated_by`

`Target` gains two writer-identity fields beside the existing
timestamps *(settled interactively 2026-07-29, refining plan
Decision 3's notes-only provenance)*:

```rust
pub created_by: String,   // "operator" | "planetarium-bridge" | future writers
pub updated_by: String,   // stamped on every write, same domain
```

- `add_target` **with** `source` stamps both with `source.kind`.
- Every operator-surface write (`add_target` without `source`,
  `update_target`, `set_goals`) stamps `"operator"`.
- Existing rows migrate to `created_by = updated_by = "operator"`
  (serde defaults; no redb schema step needed).

"Unedited since import" is now a first-class predicate:
`updated_by == "planetarium-bridge"`. The [ui-htmx targets
inbox](ui-htmx.md#targets-inbox-targets) gets "who touched
this last, and when" for free. Rich human-readable provenance (client
address, receipt time) additionally goes into `notes` as a text line
per Decision 3 — display data, never parsed.

### `add_target` import semantics (`source` present)

`source` selects a third parameter form: bare
`ra_hours` + `dec_degrees` + `source` (no `catalog_ref`, no
`display_name` — supplying either alongside `source` is an error;
naming is rp's job here). Semantics that differ from an operator add:

1. **`active: false` always** — imports land paused in the inbox
   (Decision 3); the parameter is not accepted with `source`.
2. **Proximity-only dedup** replaces the same-object slug rule: rp
   searches all stored targets for a row within
   `target_store.import.dedup_arcsec` (default `30`) of the received
   coordinates.
   - Match found, **and** it is still pending and bridge-owned
     (`!active && updated_by == source.kind`): **in-place upsert** —
     coordinates take the new value, `updated_at`/`updated_by`
     stamped, the provenance line in `notes` refreshed; slug,
     `display_name`, goals untouched. Returns `created: false`.
   - Match found, but active, operator-edited, or operator-created:
     the row is **never modified** — a new pending target is created
     with a suffixed slug. This is the Decision 3 protection, enforced
     in rp (not bridge courtesy).
   - No match: create.
3. **Goals default** from `target_store.default_goals` (Decision 10),
   as for any add without `goals[]`.
4. The `catalog_ref`-match branch of slug allocation is **never**
   consulted for imports: two imports 15′ apart that both resolve to
   "NGC 7000" are two targets (mosaic panels), not one.

### Naming — reverse cone-search at add-time

For a `source` create *(settled interactively 2026-07-29: rp
finalizes naming atomically against the store; the bridge sends bare
coordinates — this refines Decision 4's "the bridge resolves"
wording)*:

1. **Cone-search**: one query over the one logical catalog — never
   per-catalog searches *(settled 2026-07-29)*. Entries carry a
   class (deep-sky object or star), each class has its own
   acceptance radius — `naming_tolerance_arcmin` (default `10`) for
   DSOs, `star_naming_tolerance_arcmin` (default `2`) for stars —
   and a **DSO hit outranks any star hit regardless of separation**;
   separation breaks ties within a class. The tight star radius
   matches the gesture: a star deserves the name when it *was* the
   tap anchor (dead-center by construction — the faint-star-adjacent
   framing from P3a), not when it is a field star 8′ off; flat
   nearest-wins was rejected because field-star density in the
   galactic plane would systematically take names from nebula
   framings exactly where the nebulae live. A hit sets `catalog_ref`
   and denormalizes `object_type`/`magnitude`/`size_arcmin` exactly
   as a catalog add does; the display, offset, and slug rules below
   apply identically to both classes (`"HD 227018"`, slug
   `hd227018`).
2. **Display name**:
   - Hit, **and** this is the only stored target with that
     `catalog_ref`, **and** the offset from the catalog centroid is
     within `dedup_arcsec`: the plain name — `"NGC 7000"`.
   - Hit otherwise: the offset form — `"NGC 7000 +8′E −4′N"`, where
     East = Δα·cos δ and North = Δδ, each component rendered to 0.1′
     with a trailing `.0` stripped (`+8′E`, `+0.3′E`) and a
     component under 0.05′ omitted. The offset reads as *how this
     framing differs* — what the operator composed.
   - No hit: the coordinate form `"J2059+4432"` (IAU-style truncation:
     `Jhhmm±ddmm`).
   - Names are initial values only: `display_name` stays freely
     operator-editable, and existing rows are **never retroactively
     renamed** when a second framing of the same object arrives.
3. **Slug**: a hit bases the slug on the `catalog_ref` (`ngc7000`,
   suffix-allocated on collision per the landed rules — `ngc7000-2`);
   no hit uses the coordinate slug (`j2059p4432`, `p`/`m` for the
   sign). The coordinate display name matches its slug shape by
   construction.

Catalog coverage bounds *naming quality only*, never import
correctness: identity, dedup, and slug allocation are pure
coordinate proximity, so a target the catalog has never heard of
imports exactly as well as M31 — it just arrives with the coordinate
name and slug, ready for an operator rename during the
`active: false` review. Issue #767 (landed) widened `rp-catalog` from
Messier + NGC + IC to the astrophoto DSO catalogs (Sh2, Barnard,
LDN/LBN, vdB, RCW, Gum, Ced, Abell PNe, Arp, Hickson, Collinder,
Melotte, Stock, Trumpler; ~19k DSO rows) plus the full HD/HDE/HDEC
star layer from the Tycho-2/HD cross-index (~354k rows,
Tycho-2-derived J2000 positions), so the P3a faint-star-adjacent
framing gesture arrives named after its anchor — as the IAU proper
name (`"Vega"`) for the ~400 CSN-named stars, as the designation
(`"HD 227018"`) otherwise. Existing rows are never retroactively
renamed when coverage grows.

### `rp-catalog`: nearest-neighbor query

Landed with #767 (amends the earlier sketch, which borrowed
`&'a ResolvedTarget` — the catalog now materializes rows on demand
from a packed blob, so matches are owned):

```rust
pub struct NearestTolerances {
    pub dso_arcmin: f64,   // target_store.import.naming_tolerance_arcmin
    pub star_arcmin: f64,  // target_store.import.star_naming_tolerance_arcmin
}

pub struct NearestMatch {
    pub target: ResolvedTarget,    // owned; carries class: ObjectClass
    pub separation_arcmin: f64,
    pub east_offset_arcmin: f64,   // Δα·cosδ of the query FROM the centroid
    pub north_offset_arcmin: f64,  // Δδ of the query FROM the centroid
}

impl Catalog {
    pub fn nearest(&self, coord: &IcrsCoord, tolerances: &NearestTolerances)
        -> Option<NearestMatch>;
}
```

One logical search over one dec-sorted embedded structure (a
binary-searched declination band per class, well under a millisecond):
the best DSO within its radius wins outright; otherwise the best star
within its radius; separation breaks ties within a class, and *exact*
separation ties (entries packed at identical coordinates, e.g.
M 42 / NGC 1976) fall back to a fixed catalog rank
(M > NGC > IC > Sh2 > …) and then name, so the winner is
deterministic *(settled 2026-07-29 on #767)*. Deliberately *not* the
DB-seeded indexed cone-search browse that `rp-targets.md` defers; the
two must not be conflated.

### rp config additions

```jsonc
"target_store": {
  "import": {
    "dedup_arcsec": 30.0,                 // proximity-upsert window; below any mosaic panel spacing
    "naming_tolerance_arcmin": 10.0,      // DSO-class cone radius; display only, never identity
    "star_naming_tolerance_arcmin": 2.0   // star-class cone; a star names a target only when no DSO is in its cone
  }
}
```

## Configuration (bridge)

Follows the fleet conventions: durations are humantime strings, angles
bare decimal degrees, `AlpacaServerConfig` for the server block,
sentinel's `service_auth`/`ca_cert` field shape for the client wiring
(ADR-017), `resolve_and_init` minting `server.unique_id` on first
start.

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

## Doctor integration

**Landed** (with the fake-mount slice, which also registered the
bridge in doctor's embedded catalog — the `pkg/doctor.toml` alone is
not enough; `services/doctor/src/catalog.rs` must list it):

- `pkg/doctor.toml`: `class = "alpaca"`, `port = 11126`. Sentinel's
  health supervision and doctor's port checks apply as to any Alpaca
  service.
- **Client wiring**: doctor's `plan_client_wiring` provisions
  `rp.service_auth` + `rp.ca_cert` (absent-only), exactly as for
  sentinel and session-runner (ADR-017) — via nested pointers, since
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
| Slew verbs | Simulated motion only — never an import (P3b) |
| Motion verbs for `false` capabilities | ASCOM `NOT_IMPLEMENTED` |
| rp rejects `add_target` (tool error) | `error!` log; **not** spooled (would fail again) |
| rp unreachable | Spool append (`fsync` per entry); the sync verb still succeeds normally |
| Spool full | Drop oldest; `error!` per drop; `dropped_total`++ |
| Spool file unreadable at startup | `error!`, start with an empty spool (never refuse to start) |
| Corrupt spool line on replay | Skip + `error!` with the line number; continue |
| Client disconnect | Nothing arrives (P3a) — no state depends on a disconnect signal |

## MVP scope

**In scope:** the single virtual Telescope device (capability matrix
above), sync (Align) as the sole import gesture, all three slew verbs
as simulated motion, the altitude-floor reported-position policy,
live site adoption,
`assume_epoch`, the bounded spool with restart-surviving replay,
`/health`, doctor registration + the fake-mount check, ConformU clean,
and the rp-side contract (writer identity, `source` semantics,
cone-search naming, `rp-catalog::nearest`).

**Deferred:**

- Sentinel scraping `/health` (follow-up once a second consumer wants
  it).
- `position_angle_degrees` on imports — the field landed with P2
  (rp.md § Target Store → Position angle) but is **permanently
  operator-owned**: rp rejects it alongside `source`, imports always
  land inheriting the train default, and the [ui-htmx targets
  inbox](ui-htmx.md#targets-inbox-targets) is where per-target angles
  are entered — SkySafari cannot export its FOV angle through any
  channel (P3a/Decision 5).
- Stellarium/CdC enrichment (P5/P6 — both can use this device
  unenriched meanwhile).
- Retroactive display-name disambiguation of earlier imports.
- Any per-client identity beyond the provenance stamp (`ClientID` is
  unstable across app contexts — P3a).

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
| `spooling.feature` | rp down → spool; replay in order on recovery; replay after restart; overflow drops oldest with counter; corrupt line skipped; tool-error not spooled |
| `doctor.feature` | The shared doctor smoke (testing.md § Doctor smoke): a valid config yields a clean report, an unknown key fails the report and is named. Rides `bdd_infra::doctor_smoke`; the world's `valid_config()` spells out all five config blocks, so the clean-report scenario covers the whole typed load path |
| `auth.feature` | The shared TLS + auth smoke (testing.md § TLS/auth): with `server.tls` and `server.auth` configured the bridge serves HTTPS and answers 401 without credentials, 200 with them. Rides `bdd_infra::tls_auth`; the world's launch hook points `spool.path` at the scenario temp dir, since an absent path resolves to — and creates — the platform config dir |

rp-side additions (rp's suite): import creates pending with writer
identity; proximity upsert of a pending-unedited import; active /
operator-edited / operator-created rows never mutated (suffixed slug
instead); mosaic-spaced GoTos stay distinct; plain vs offset vs
coordinate display names; goals defaulted; `source` +
`catalog_ref`/`display_name` rejected. `rp-catalog::nearest` gets
unit tests (hit/miss/tie, class rank — a DSO outranks a nearer
star, a star wins only a DSO-less cone — offset vector signs,
per-class tolerance edges).

ConformU runs under `bazel test --config=conformu` per the existing
mock-backend pattern.

## P3b horizon experiment — closed 2026-07-30

Run against the spike with a second device (identical SkySafari
build); full findings in the
[P3b appendix](#appendix-p3b-horizon-experiment-findings-2026-07-30).
The three questions resolved: (1) an object-GoTo to a below-horizon
target is refused under **every** horizon display setting — the gate
is unconditional; (2) the wedge did **not** reproduce on the second
device under any setting — device/state-dependent, cause
unattributed, floor policy retained; (3) coordinate-entry GoTo **is**
horizon-gated below 0° — P3a's "ungated" observation had only ever
reached an above-horizon point. Net consequence: GoTo cannot express
below-horizon planning at all, which drove Decision 2's second
amendment (§ [Align is the import gesture](#align-sync-is-the-import-gesture)).

---

## Appendix: P3a verification-spike findings (2026-07-29)

Session: 2026-07-29 (UTC), **SkySafari Pro 8.0.3 (build 1205)** on iPad
driving the spike over Wi-Fi, ~20 minutes of traffic; the JSONL wire
log is the raw evidence (kept off-repo with the operator). Findings are
from this one client/version; the plan's SkySafari floor is v7
(Decision 1), not separately tested. The spike crate
`spikes/planetarium-bridge-p3a` (throwaway, sanctioned per Decision 8 /
ADR-005) stayed runnable through the P3b experiment above and was
deleted when the bridge implementation landed.

### The P3a questions — answered

| # | Question | Verdict | Finding |
|---|----------|---------|---------|
| 1 | Discovery and connection lifecycle | Answered | Alpaca UDP discovery is **subnet-local** (broadcast; does not cross routed segments — the iPad and spike host were on different subnets and no discovery datagram ever arrived). Manual IP:port entry works across subnets, confirming the plan's documented-default posture. Lifecycle detail below. |
| 2 | Which slew/sync verbs | Answered | GoTo = **`SlewToCoordinatesAsync`, exclusively** (never the blocking variant, never `SetTarget*`+`SlewToTarget`). Align = **`SyncToCoordinates`** (never `SyncToTarget`). The verbs are cleanly distinct — Decision 2's sync-is-not-intent rule is safe. Numbers arrive in **scientific notation** (`RightAscension=1.341988e+01`); parsers must accept it (our `ascom-alpaca` fork does). |
| 3 | J2000 honored, or JNow? | Answered | **J2000 honored.** Five object GoTos (Spica, M 92, M 13, Draco Dwarf, HD 142596) all matched the target's J2000 position to arcseconds; the automated probe verdict on M 13 read 0.04′ (J2000 frame) vs 12.13′ (JNow frame). `EquatorialSystem` is read **once, at connect** — changing the declaration requires a reconnect. No epoch setting exists anywhere in SkySafari's scope UI, so this reads as client behavior (honor the device declaration), not per-install configuration; the `assume_epoch` override stays as a safety valve. |
| 4 | Position-report cadence | Answered | A steady **1 Hz cycle** of `Slewing` → `Tracking` → `RightAscension` → `Declination` (four GETs ~10 ms apart, every ~1.0 s), identical while idle and while slewing. The spike's 3 s simulated convergence satisfied it: SkySafari showed the slew as arrived when `Slewing` flipped false. No connection timeouts observed. |
| 5 | **Go/no-go: arbitrary-point GoTo** | **GO, with caveats** | Tapping empty sky offers **no** GoTo — an object must be selected. But **coordinate entry (Search → coordinates) exists and GoTos arbitrary points**. Caveat: the entry form has **no epoch choice and interprets input as equinox-of-date (JNow)**, converting to J2000 on the wire — proven by a round-number probe: entered 14h00m00s / −40°00′00″ arrived as 13h58m23s / −39°52′11″, which is exactly the J2000 equivalent of the entered values read as JNow (the RA −1m37s / Dec +7.7′ shifts match 26.6 years of precession at that position to the second). A second practical path: SkySafari's selectable catalog reaches faint HD/Tycho stars and obscure PGC galaxies — a star within arcminutes of any intended frame center almost always exists and GoTos with exact J2000 coordinates. |

### Connection lifecycle detail

- **Preset editor probe** (before any connect): `apiversions` →
  `configureddevices` → `apiversions` → `alignmentmode` →
  `canslewasync` → `canslewaltazasync`, under throwaway `ClientID`s.
- **Connect**: `PUT Connected=true`, then a property battery —
  `EquatorialSystem` (once), site latitude/longitude, `UTCDate`, and a
  capability sweep (`CanSync`, `CanSyncAltAz`, `CanSetTracking`,
  `CanPark`, `CanMoveAxis`, `CanSlewAsync`, `CanSlewAltAzAsync`) — then
  the 1 Hz poll loop.
- **SkySafari pushes site and time to the device**: `PUT SiteLatitude` /
  `PUT SiteLongitude` (its own GPS-derived values) and `PUT UTCDate`.
  The spike accepted the site writes; `SetUTCDate` answered
  `NOT_IMPLEMENTED` (1024) — SkySafari retried once ~23 s later and
  carried on unaffected.
- **Disconnect sends nothing**: polling simply stops — no
  `Connected=false` was observed. The bridge must not depend on an
  explicit disconnect signal (idle-timeout thinking only).
- `ClientID` is **not stable** across app contexts: the main scope
  panel used one value, other gestures another. Treat it as
  diagnostic, not identity.
- SkySafari's **Center** button is display-only (no wire traffic) —
  only **GoTo** and **Align** reach the device.

### Below-horizon behavior

- **GoTo is horizon-gated client-side.** A below-horizon target's GoTo
  is refused by SkySafari — sometimes silently (nothing on the wire, no
  dialog), sometimes with a warning dialog. Couch-planning an object
  that has not yet risen therefore **cannot** be imported by object-GoTo
  at that moment; the operator must import while the target is up, or
  use coordinate entry (not horizon-gated in the observed case, which
  reached a 17°-altitude point).
- **Align/Sync was NOT horizon-gated** — a sync to below-horizon M 31
  went through (moot for the real bridge: sync is now rejected).
- **Wedge hazard**: after that sync put the virtual scope's reported
  position below the horizon, SkySafari refused *every* subsequent GoTo
  ("stuck") until the reported position returned above the horizon
  (fixed server-side with a corrective sync). This drove the
  [reported-position policy](#reported-position-policy-the-altitude-floor):
  the hazard exists with no sync at all, since a tracked position
  imported at dusk sets hours later.

### Design implications carried into this document

1. Manual IP:port is the primary connection story → § Discovery.
2. The GoTo/Sync verb split is exactly as Decision 2 assumed; sync
   carries zero intent → sync was rejected at design time (chosen over
   the plan's accept-and-ignore, 2026-07-29; **reversed by P3b** —
   § Align is the import gesture).
3. The wire is J2000 end-to-end for this client → `assume_epoch`
   default `"j2000"`, kept as cheap insurance.
4. Site/UTC pushes and scientific-notation floats → § Site and time
   writes.
5. Reported position must never linger below the horizon → § the
   altitude floor.
6. SkySafari's coordinate-entry box is JNow (no epoch choice) —
   operator docs must warn that J2000 coordinates typed there land
   ~20′ off; framing via a nearby faint catalog star avoids the
   pitfall entirely.
7. No explicit disconnect arrives → no bridge state may depend on one.

---

## Appendix: P3b horizon-experiment findings (2026-07-30)

Session: 2026-07-30 (UTC), **SkySafari Pro 8.0.3 on a phone — the
same app build as P3a's iPad** — driving the same spike over Wi-Fi
(manual IP:port across routed subnets, as before); the JSONL wire log
is the raw evidence (kept off-repo with the operator). The build
identity matters: every P3a/P3b behavioral difference below is
device- or state-dependent, not a version difference.

### The P3b questions — answered

| # | Question | Verdict | Finding |
|---|----------|---------|---------|
| 1 | Object-GoTo below horizon still refused with horizon display off/transparent? | **Refused, always** | NGC 253 (≈ −17° to −20° alt) GoTo refused with the horizon off, transparent, and on. Nothing reaches the wire (client-side refusal). The gate is unconditional. |
| 2 | Does the reported-position wedge still occur? | **Did not reproduce** | After a below-horizon Align (reported position ≈ −20°), an object GoTo to the Moon (≈ 24° up) went through — under every horizon display setting. The P3a iPad wedged on the identical build. Device/state-dependent; unattributed. The altitude floor stays as defense. |
| 3 | Coordinate-entry GoTo still ungated below 0°? | **Gated** | RA 6h / Dec +20° (≈ −37° alt) refused with "command failure cannot go to". Overturns P3a's impression — its coordinate-entry probe had only ever reached a +17°-altitude point, so the below-0° case was never actually exercised until now. |

### Additional findings

- **Align is never horizon-gated.** Three below-horizon
  `SyncToCoordinates` for NGC 253 arrived J2000-exact
  (0.79253h / −25.2875°), matching the catalog centroid to the
  arcsecond each time.
- **Align is object-only**: SkySafari offers no Align on an entered
  coordinate point, so arbitrary-point import rides the
  faint-star-adjacent anchor gesture at any altitude.
- **The faint-star anchor gesture works end-to-end.** An Align on a
  mag 8.15 field star near NGC 6633 arrived 0.12″ from the packed
  catalog's Tycho-2-derived position for **HD 170881**; run through
  `rp_catalog::nearest()` (post-#767 catalog, 10′/2′ tolerances) the
  point names as `HD 170881` dead-center, while NGC 6633 — 62′ away,
  outside its DSO cone — correctly does not outrank the anchor. A
  second Align on a mag 9.27 star with nothing else in frame resolved
  identically (**HD 172011**, 0.12″): the anchor works in empty
  fields too, and SkySafari's selectable depth comfortably reaches
  the HD layer's magnitude range.
- **Center is display-only** (crosshair motion, zero wire traffic) —
  re-confirmed; it is the visual-confirmation half of the
  Center → Align workflow.
- The coordinate-entry form exposed **no minus sign** for declination
  in this session — the below-horizon probe used a positive-dec
  anti-meridian point instead. (P3a *did* enter −40°, so the sign
  control's availability varies with device or context.)
- Two Moon GoTos 16 minutes apart arrived with coordinates drifted
  ~8′ — solar-system GoTos carry live positions.

### Design implications carried into this document

1. GoTo cannot import below-horizon targets under any client setting
   → the import gesture is **Align** (Decision 2's second amendment,
   § Align is the import gesture); slews are simulated motion only.
2. The wedge is real (P3a) but not reliably reproducible (P3b) → the
   altitude floor stays, default `10.0`, `null` documented as safe
   for wedge-free clients.

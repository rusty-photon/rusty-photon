# ui-htmx Service (web BFF)

## Overview

`ui-htmx` is the browser-facing, **server-rendered web UI** for rusty-photon —
the web UI described in
[`docs/plans/archive/config-actions.md`](../plans/archive/config-actions.md)
with the chosen visual direction in
[`docs/plans/ui-design/mocks/README.md`](../plans/ui-design/mocks/README.md). It
is a **standalone backend-for-frontend (BFF)**: a client of the rest of the
system that holds no UI logic inside `rp` (`rp.md` tenet 7). The service renders
HTML on the server with [axum] + [Maud] and adds interactivity with [HTMX]; there
is no npm, no WASM, no client-side framework.

It serves four surfaces, one nav:

1. **Configuration** (`/`, deep pages at `/config/{service}`) — `/` *is*
   rp's settings page (the same schema-driven form `/config/rp` serves).
   Per-device configuration is reached from the equipment page's Configure
   buttons, not from here — the device list lives in one place.
   `/config/{service}` resolves the two target kinds: the literal `rp` and
   roster-derived `rp:{kind}:{id}` (rp's equipment roster is the **only**
   device source — #569, ADR-016 amendment 6; there is no static drivers
   map).
2. **Equipment page** (`/equipment`) — `rp`'s equipment roster: live
   connection state, a managed/foreign capability tier per device, and
   add / edit / remove of roster entries by editing `rp`'s config over REST.
3. **Targets inbox** (`/targets`) — rp's [Target
   Store](rp.md#target-store) as an operator surface: review pending
   (paused) targets — typically planetarium imports
   ([planetarium-bridge.md](planetarium-bridge.md)) — edit their
   acquisition goals and framing position angle, and activate them into
   the planner's rotation or discard them. The BFF's first **MCP**
   surface: target CRUD is deliberately MCP-only on rp (rp.md Tenet 8),
   so this page drives rp's target tools through
   [`rp-mcp-client`](../decisions/017-standard-mcp-client-construction.md).
4. **Activity stream** (`/stream`) — the live session narrative from the
   [`7-stream-fold.html`](../plans/ui-design/mocks/7-stream-fold.html) mock:
   `rp`'s real-time event stream rendered server-side and pushed to the
   browser over SSE.

The [`rp` target](#configuration) is **required** — every surface is
rp-backed, so an rp-less BFF has no purpose and a config without the block
fails loudly at load.

**JavaScript (htmx) is required.** The UI does not carry a no-JS fallback: the
form submits via `hx-post` (no `method`/`action`), and the unlock/lock/retry
affordances are `<button hx-get>` (no `<a href>`), so without htmx loaded the page
renders but is inert. This is a deliberate decision (UI-testing plan §7): the UI is
**optional** — rusty-photon runs fully headless — and the genuine recovery path is
ssh + editing the config file, strictly more capable than a degraded web form; a
whole-app no-JS guarantee is also incompatible with the future real-time stream UI.
Direct navigation/refresh still returns a full styled page (the `HX-Request`
full-page-vs-fragment branch is core htmx, not a no-JS feature).

It renders a configuration page for **any** rostered device's driver,
**generated from the driver's own JSON Schema** (`config.schema`) rather than
a hand-built form: read the driver's current configuration, edit it, and
apply changes — all by calling the driver's `config.get` / `config.schema` /
`config.apply` ASCOM actions over HTTP (the cross-driver protocol; see
[`config-actions.md`](config-actions.md)). One BFF serves every device in
rp's roster, each addressed under `/config/rp:{kind}:{id}`.

Phase 2 shipped a hand-built `dsd-fp2`-only page; Phase 3b (this design) replaced
the hardcoded field lists with a **schema-driven renderer** that walks any
driver's `config.schema` into a form and reads its editability tiers
(`locked_fields` / `read_only_fields`) from the schema, so a new driver needs **no
BFF changes** to get a config page.

[axum]: https://github.com/tokio-rs/axum
[Maud]: https://maud.lambda.xyz/
[HTMX]: https://htmx.org/

## Naming and the `ui-*` family

This crate is the first member of a `ui-*` family of UI expressions. The naming
scheme distinguishes UI expressions along two axes — **technology** (for browser
targets) and **target** (for native):

| Crate | Target | Technology | Status |
|-------|--------|------------|--------|
| **`ui-htmx`** | browser | server-rendered HTMX | **this crate** |
| `ui-leptos` | browser | Leptos / WASM | future |
| `ui-ios` | iOS | native | future |
| `ui-android` | Android | native | future |
| `ui-core` | — | shared backend-for-frontend logic | extract when expression #2 lands |

A tech name (`htmx`, `leptos`) implies the browser target; a target name (`ios`,
`android`) implies native delivery. With HTMX the BFF and the frontend are
**fused** — the server renders the HTML directly — so `ui-htmx` is simultaneously
"the web BFF" and "the HTMX frontend". When a second expression appears, the
driver-client + config-model logic (target/tech-agnostic) is extracted into
`ui-core`; it would be premature with a single consumer today.

## Architecture

```
                 ┌────────────────────────────────────────┐
   browser ────► │  ui-htmx  (services/ui-htmx)            │  server-rendered HTML
   (HTMX)        │                                         │
                 │  main.rs ─► lib.rs (build_router)        │
                 │      │                                   │
                 │      ▼                                   │
                 │  pages/  (Maud templates + HTMX attrs)   │
                 │      │  renders form / fragments         │
                 │      ▼                                   │
                 │  ConfigClient (driver_client.rs)         │
                 │      │  get_config() / apply_config()    │
                 │      │  speaks the ASCOM action protocol │
                 │      ▼                                   │
                 │  HttpClient (io.rs)                      │
                 │      │  get() / put_form()  (reqwest,    │
                 │      │  rusty-photon-tls CA trust + Basic auth)    │
                 └──────┼──────────────────────────────────┘
                        │  PUT /api/v1/covercalibrator/0/action
                        │     Action=config.get | config.apply
                        ▼
                  [ dsd-fp2 ]   (ASCOM Alpaca driver, port 11119)
```

Two thin, independently mockable seams keep the handlers testable without a live
driver (the pattern `sentinel` uses for its Alpaca polling — see
[`sentinel.md`](sentinel.md)):

- **`HttpClient`** (`io.rs`) — `get(url)` / `put_form(url, params)`. Production
  impl wraps `reqwest` and is built through `rusty_photon_tls::client::build_reqwest_client`
  so it trusts the Rusty Photon CA, with optional HTTP Basic auth. Requests send
  `Connection: close` (no keep-alive pooling): a driver applies config by
  reloading — tearing its server down and rebinding — which leaves a pooled
  connection stale, and a non-idempotent `PUT` is not retried. A fresh connection
  per request lets the reconnect poll recover the instant the driver is back;
  config actions are low-frequency, so the lost pooling is immaterial. Mocked
  with `mockall` for unit tests of the layer above.
- **`ConfigClient`** (`driver_client.rs`) — `get_config()` / `get_schema()` /
  `apply_config(Value)`. Knows the ASCOM action protocol: shapes the
  `PUT .../action` request, unwraps the Alpaca envelope, and parses the inner
  JSON into the shared `rusty_photon_config::actions` wire types. The page handlers depend on `Arc<dyn ConfigClient>`, so a handler unit
  test can inject a stub (via `AppState::with_client`) to cover an error state
  a live driver won't produce — see [Testing Strategy](#testing-strategy). The
  end-to-end BDD suite, by contrast, runs against a real driver, not a stub.

### The driver config-action client (wire contract)

Each driver exposes config over the standard ASCOM `Action` mechanism. The BFF
calls:

```
GET  /api/v1/{type}/{n}/supportedactions
   → Alpaca envelope, Value = ["config.get","config.apply", …]

PUT  /api/v1/{type}/{n}/action
       Action=config.get      Parameters=     ClientID=… ClientTransactionID=…
   → Alpaca envelope, Value = "<ConfigGetResponse as a JSON string>"

PUT  /api/v1/{type}/{n}/action
       Action=config.apply    Parameters=<full Config JSON>   ClientID=… …
   → Alpaca envelope, Value = "<ConfigApplyResponse as a JSON string>"
```

The **Alpaca envelope** wraps every response:

```jsonc
{ "Value": <result>, "ClientTransactionID": 0, "ServerTransactionID": 12,
  "ErrorNumber": 0, "ErrorMessage": "" }
```

`AlpacaConfigClient` parsing rules:

1. **HTTP non-2xx** → transport error (the driver's auth/TLS rejected us, or it is
   down). Rendered as a "driver unreachable / refused" banner.
2. **`ErrorNumber != 0`** → an ASCOM action error. `0x40C` (1036,
   `ACTION_NOT_IMPLEMENTED`) means the target is not a config-capable driver;
   surfaced as "this driver does not expose configuration". Other codes surface
   `ErrorMessage`.
3. **`ErrorNumber == 0`** → `Value` is a **JSON string**; parse it into the typed
   `ConfigGetResponse` / `ConfigApplyResponse`. (For `supportedactions`, `Value`
   is a JSON array, not a string.)

For `config.get` the inner body is `{ "config": <effective Config, secrets
redacted>, "overrides": ["serial.port"] }`. For `config.apply` it is the
classification body documented in [`dsd-fp2.md`](dsd-fp2.md) "config.apply"
(`status`, `applied`, `reload`, `restart_required`, `skipped_override`,
`persisted_to`, `errors`).

The config blob is treated as an **opaque `serde_json::Value`** by the transport
layer; the page discovers field paths from the driver's `config.schema` at
request time, so it hardcodes **no** driver-specific field knowledge. This keeps
the BFF decoupled from every driver crate — it depends only on the light,
driver-agnostic `rusty-photon-config` crate for the shared wire types, and pulls
in no driver's serial/transport dependencies.

## Routes

The config routes are **service-scoped** (`{service}` is the literal `rp` or
a roster-derived `rp:{kind}:{id}` key — see
[Config-page targets](#config-page-targets)), so one BFF serves rp and every
rostered device.

| Method | Path | Purpose |
|--------|------|---------|
| `GET`  | `/` | The Configuration surface: rp's settings page — identical to `GET /config/rp`, form posts and restart affordances included (device links live on the equipment page as its per-device Configure buttons). |
| `GET`  | `/config/{service}` | Call `config.schema` + `config.get`; render the form generated from the schema, filled with current values. An optional `?unlock=<field>` query renders one locked/identity field (e.g. a device `unique_id`) editable — the read-only-by-default escape hatch. Resolve failures render honest, distinct cards: unknown `{service}` ("no configured driver"), rp unreachable while resolving a roster key (retryable), or a roster entry no client can be built from (e.g. malformed `alpaca_url` — links to the Equipment page to fix it). |
| `POST` | `/config/{service}` | Re-fetch `config.schema` to coerce the form back into the full Config, call `config.apply`; render the result state (see below). |
| `GET`  | `/config/{service}/status` | HTMX poll target during reconnect: try `config.schema` + `config.get`; when the driver answers, swap in the refreshed form. Honours the same optional `?unlock=` query. |
| `POST` | `/config/{service}/restart` | Ask Sentinel to restart the target's process and render the outcome (see [Restart via Sentinel](#restart-via-sentinel-post-configservicerestart)). The Sentinel-side name is `rp` for rp's own page; for a roster-derived device it is the discovered service whose `probe_port` matches the device's `alpaca_url` port. |
| `GET`  | `/equipment` | The [equipment page](#equipment-page-equipment): rp's roster with live connection LEDs, capability tiers, and add/edit/remove affordances. |
| `GET`  | `/equipment/{kind}/new` | Schema-generated "add device" form for one equipment kind (`cameras`, `filter_wheels`, `cover_calibrators`, `focusers`, `safety_monitors`, `switches`, `rotators`, `observing_conditions`, `domes`, `mount`). |
| `POST` | `/equipment/{kind}/new` | Insert the new entry into rp's config (`GET /api/config` → splice → `PUT /api/config`); render the roster with the apply outcome. |
| `GET`  | `/equipment/{kind}/{id}/edit` | Edit form for one roster entry, prefilled from rp's config (the singular `mount` uses the fixed id `mount`). |
| `POST` | `/equipment/{kind}/{id}/edit` | Replace that entry in rp's config and apply. |
| `POST` | `/equipment/{kind}/{id}/delete` | Remove that entry from rp's config and apply. |
| `GET`  | `/targets` | The [targets inbox](#targets-inbox-targets): pending targets awaiting review plus the active roster, from rp's `list_targets` MCP tool joined with the filter roster and train position-angle defaults from `GET /api/config`. |
| `GET`  | `/targets/{slug}` | The per-target review form: editable display name, priority, position angle (blank = inherit), notes, and the acquisition-goals editor; coordinates and provenance are display-only. |
| `POST` | `/targets/{slug}` | Save the review form: `update_target` (scalar fields, with the position angle's blank ⇒ explicit-`null` mapping) then `set_goals` (full replacement); rp-side validation errors re-render field-level with values preserved. |
| `POST` | `/targets/{slug}/active` | Activate (`active=true` — accept a pending target into the rotation) or pause (`active=false`) via `update_target`. |
| `POST` | `/targets/{slug}/delete` | Discard the target via `delete_target`. |
| `GET`  | `/targets/goal-row` | Goal-editor fragment: a blank goal row for the "Add goal" affordance, or an empty body for the per-row "Remove" swap. |
| `GET`  | `/stream` | The [activity stream](#activity-stream-stream) page. |
| `GET`  | `/stream/events` | The SSE proxy: rp's event stream rendered as HTML fragments (see [SSE proxy](#the-sse-proxy-streamevents)). |
| `GET`  | `/stream/equipment` | Fold-panel equipment-LED fragment; the panel re-fetches it on an htmx timer. |
| `GET`  | `/health` | Liveness; returns `OK`. |
| `GET`  | `/assets/app.css`, `/assets/htmx.min.js`, `/assets/htmx-ext-sse.js` | Embedded static assets (`include_str!`). The SSE extension ([htmx-ext-sse] 2.2.3, vendored) is loaded only by pages that stream. |

[htmx-ext-sse]: https://github.com/bigskysoftware/htmx-extensions/tree/main/src/sse

Every page shares the [`layout`] shell, whose top nav carries the four
surfaces — **Activity** (`/stream`), **Equipment** (`/equipment`),
**Targets** (`/targets`), **Configuration** (`/`) — with the active tab
highlighted, plus the mock's pure-CSS **night-vision toggle** (a
page-level red filter preserving dark adaptation; no JavaScript).

[`layout`]: ../../services/ui-htmx/src/pages/mod.rs

### Schema-driven rendering (`FieldModel`)

The form is **generated from the driver's `config.schema`**, not a hardcoded
field list. `FieldModel::from_schema` walks the JSON Schema into a flat, ordered
list of scalar leaves:

- **`$ref` into `$defs` is resolved**, and plain objects are recursed, so nested
  config sections (`serial.port`, `server.discovery_port`, …) become dotted leaf
  paths.
- **`oneOf` / `anyOf` / `allOf` / `enum` / `const` subtrees are skipped** — an
  optional nested struct (`Option<TlsConfig>`/`Option<AuthConfig>`), a tagged
  enum (`star-adventurer`'s `transport`), or a custom-serde type is not rendered
  as editable inputs; it **round-trips untouched** through the hidden blob. This
  is exactly how redacted secrets (which live inside such optional structs) stay
  safe — they are never rendered, only carried through.
- **One array shape renders: `array` whose `items` are an integer `enum`**
  (e.g. rp's per-camera `cooler_targets_c` grid) becomes a **checkbox
  group** — one checkbox per enumerated value, in the schema's `enum` order,
  checked when the config array contains the value; submitted values are
  written back in `enum` order. Nothing field-specific lives in the BFF: the
  allowed values come from the schema. Every other array (arrays of objects,
  `$ref` items) still skips and round-trips via the blob.
- Each leaf's **`FieldKind`** is inferred from the schema: `string` → text,
  `boolean` → checkbox, `integer`/`number` → numeric input, integer-enum
  array → checkbox group, with the schema's `minimum`/`maximum` and
  nullability (`type:["integer","null"]`) driving coercion. Fields are
  grouped into a `<fieldset>` per top-level section.

The form edits a subset of fields; the rest must round-trip unchanged so
`config.apply` receives a complete `Config`. The page therefore carries the full
`config.get` blob (already secret-redacted) in a **hidden field**, and on POST
re-fetches `config.schema` to rebuild the `FieldModel`, then overlays each
editable leaf onto the blob by JSON pointer and sends the merged value as
`Parameters` to `config.apply`. This is the round-trip the protocol was designed
for:

- **Override-pinned fields** (reported in `config.get`'s `overrides[]`) are
  rendered **read-only** with an explanation; the driver skips them on persist
  regardless (`skipped_override[]`), so even though the hidden blob carries the
  effective value, a transient `--port` is never baked into the file.
- **Redacted secrets** are never rendered (they live inside the `anyOf`/optional
  subtrees the walker skips) and round-trip as the `********` sentinel in the
  hidden blob; `config.apply` treats the sentinel as "leave unchanged", so a
  saved form never blanks a password hash.
- **Numeric fields are parsed into their bounded types** (`u16` ports, `u32`
  baud/brightness). An out-of-range or non-numeric value becomes a field-level
  error (re-rendered, not sent), rather than silently coercing to `0` or
  producing a non-field driver parse error. An empty *required* number keeps the
  prior value (clearing a port can't silently become OS-assigned); an empty
  *optional* number (`discovery_port`) persists `null`.
- **Clearing an optional field unsets it, in the spelling that means it.**
  A cleared *optional* text box persists `null`, exactly as a cleared optional
  number does — `""` and absent are different states, and only `null` is the
  one that means "unset". Drivers are entitled to distinguish them: rp reads an
  absent `session.file_naming_pattern` as "no templated naming" but an empty
  one as malformed, so sending `""` would turn a clear into a config rp
  refuses to load. A cleared *required* text box still sends `""`, leaving the
  driver to reject its own empty required field.
- **Read-only fields come from the driver, not a BFF list.** The hard-read-only
  tier is whatever the driver reports in `config.schema`'s `read_only_fields`
  (e.g. `server.port` — a rebind the BFF can't follow — and a device `enabled`
  flag — disabling the device unregisters the very endpoint the config actions
  live on). The BFF renders these disabled and `merge_form` never overlays them
  (they round-trip from the blob), so the UI can't edit away its own
  reachability. A new driver decides its own self-lockout guards by listing them
  in `read_only_fields`; the BFF needs no change. (This governs the **UI path**
  only — a hand-crafted POST that edits a read-only field inside the `__config`
  blob is equivalent to any forged config and is the driver's job to reject.)

#### Field-editability tiers

The form classifies each field into one of four tiers, evaluated in this order
(the first that applies wins for the `disabled` state, and `merge_form` mirrors
the same precedence when deciding whether to overlay a submitted value). **Every
tier is sourced from the driver** — `config.get`'s `overrides[]` and
`config.schema`'s `locked_fields` / `read_only_fields` — never a BFF-side list:

| Tier | Source | Disabled? | Overlaid by `merge_form`? |
|------|--------|-----------|---------------------------|
| **Override-pinned** | `config.get` `overrides[]` (CLI flags) | yes | never (driver skips it anyway) |
| **Hard read-only** | `config.schema` `read_only_fields` (e.g. `server.port`, a device `enabled`) | yes, always — no escape hatch | never |
| **Locked / identity** | `config.schema` `locked_fields` (e.g. a device `unique_id`) | yes **by default**; no once unlocked | only when unlocked **and** not pinned |
| **Editable** | every other schema leaf | no | yes (unless pinned/read-only) |

Pinned always wins: an override-pinned field stays read-only even if it is also a
locked/identity field that the user unlocked.

- **A device `unique_id` is a *locked / identity* field — read-only by default
  behind a deliberate escape hatch**, distinct from the hard read-only tier
  above. The driver **owns and generates** its ASCOM `UniqueID`, so editing it
  from the page is an escape hatch for a *misbehaving driver*, not routine
  configuration. By default the field renders **disabled** with the hint
  *"Identity — the driver owns this. Editing is an escape hatch for a misbehaving
  driver."* and an **"Unlock to edit"** link
  (`GET /config/{service}?unlock=<field>`). Following it re-renders the same card
  with the field **enabled**, a warning, and a **"Lock again"** affordance
  (`GET /config/{service}`, no query). The unlock state is carried with **no
  bespoke client-side JavaScript** (htmx performs the GET + swap; there is no
  hand-written JS):
  - On a **GET**, the `?unlock=<field>` query (axum `Query`) names the field to
    unlock; only a name in the schema's `locked_fields` is honoured (a
    hard-read-only field, a typo, or no query unlocks nothing).
  - The rendered card emits a hidden `__unlocked` field
    (`serde_json::to_string` of the unlocked set) alongside `__config` /
    `__overrides`, so on **POST** the unlocked set round-trips. `merge_form`
    overlays a locked field from its form value **only if** `__unlocked` lists it
    **and** it is not override-pinned; otherwise it round-trips from the hidden
    blob untouched. An invalid submission re-renders with the field still
    unlocked (the operator's in-progress edit is preserved); a successful apply
    re-locks it. Unlike `__config` / `__overrides` (required and validated),
    `__unlocked` is **optional** and a malformed value is treated as "nothing
    unlocked" — the safe default keeps the field read-only, and the overlay gate
    still requires the name to be present, so a forged or absent `__unlocked` can
    never *edit* a locked field. The set is filtered to the schema's
    `locked_fields`, so a forged `__unlocked` can only ever unlock a field that is
    genuinely a locked/identity field — never a hard-read-only one.

  (As with the read-only tier, this governs the **UI path** only. A hand-crafted
  POST that edits a locked field inside the `__config` blob is equivalent to any
  forged config and is the driver's job to reject — driver-side identity
  validation lands separately.)

## Config-page targets

`/config/{service}` resolves its target in two ways; the page machinery
(schema walk, tiers, merge, apply states) is identical for both. (The former
third kind — a static `drivers`-map entry — was retired by #569: rp's roster
is the only device source.)

1. **`rp` itself** — the literal key `rp`; the client is `RestConfigClient`
   speaking the same protocol as plain REST (`GET /api/config`,
   `GET /api/config/schema`, `PUT /api/config` — see
   [`config-actions.md`](config-actions.md) "REST transport"). Because rp
   classifies every change as `restart_required` (it has no in-process
   reload), the apply result renders the **restart callout** instead of the
   reconnect poll: "Saved — restart rp to apply:" plus the changed paths,
   with the "Restart via Sentinel" affordance inline. rp's equipment arrays
   are `oneOf`-free but *array-typed* — arrays of objects, which the schema
   walker skips — so on the rp config page they round-trip untouched via the
   hidden blob, and are edited on the
   [equipment page](#equipment-page-equipment) instead (where a camera
   entry's integer-enum `cooler_targets_c` array renders as a checkbox group
   like any other walked field). rp's optional blocks
   (`site`, `guider`, `plate_solver`, `planner`) blob-round-trip the same way
   under the standard composite-skip rule; the page edits the scalar leaves
   (`session`, `safety`, `imaging`, `centering`, `cooling`, `server`).
2. **Roster-derived device** — a key of the form `rp:{kind}:{id}` (e.g.
   `rp:cameras:main-cam`, `rp:mount:mount`), synthesized on demand from rp's
   config: the device's `alpaca_url` + device number come from its roster
   entry, and the ASCOM device type from which array it sits in
   (`cameras`→`camera`, `filter_wheels`→`filterwheel`,
   `cover_calibrators`→`covercalibrator`, `focusers`→`focuser`,
   `safety_monitors`→`safetymonitor`, `switches`→`switch`, `rotators`→`rotator`,
   `observing_conditions`→`observingconditions`, `domes`→`dome`,
   `mount`→`telescope`). The BFF calls the
   device **without credentials** (rp redacts per-device auth, rightly), so a
   driver behind auth renders the transport-error banner; the doctor-minted
   service credential (D6) is the path to authenticated devices, not a
   second device list.

## Behavioral contracts

### Rendering the page (`GET /config/{service}`)

- **Unknown service:** a `{service}` that is neither `rp` nor a roster
  entry's key → render an error card ("No configured driver named …").
- **Happy path:** `config.schema` + `config.get` succeed → render the
  schema-generated form filled with the effective config. Fields listed in
  `overrides[]` are disabled and annotated "pinned by a command-line override".
- **Locked/identity escape hatch:** a `locked_fields` entry (e.g. a device
  `unique_id`) is disabled by default with an "Unlock to edit" link.
  `GET /config/{service}?unlock=<field>` re-renders the card with that locked
  field editable (only names in the schema's `locked_fields` are honoured); the
  no-query URL re-locks it. See
  [Field-editability tiers](#field-editability-tiers).
- **Driver unreachable / refused:** `HttpClient` transport error or HTTP non-2xx
  (on either `config.schema` or `config.get`) → render an error banner naming the
  driver URL, with a retry link. The form is not shown (there is nothing to edit).
- **Non-config driver:** `ErrorNumber == ACTION_NOT_IMPLEMENTED` → render an
  explanation that the target driver does not expose configuration actions.

### Applying changes (`POST /config/{service}`)

- **`status:"applying"`** (a field needed a reload): render a "Saved — the driver
  is reloading…" state that **polls** `GET /config/{service}/status` via
  `hx-trigger="every 1s"`. When the poll's `config.get` succeeds, swap the
  reconnecting fragment for the refreshed form plus a "reconnected" confirmation.
  This is the same brief blip a process restart would cause; the BFF treats it as
  expected (see the reload mechanics in the plan).
- **`status:"ok"`** (persisted, nothing needed a reload): render "Saved." with the
  refreshed form; no reconnect poll. When `restart_required[]` is non-empty
  (the `rp` target — `ApplyDisposition::Restart`), the banner becomes the
  **restart callout**: "Saved — restart rp to apply:" with the changed paths
  listed; the form re-renders from the *running* (pre-restart) config, which is
  honest about what is currently in effect.
- **`status:"invalid"`** (validation failed, file unchanged): re-render the form
  with each `errors[]` entry shown next to its field (`path` → field), preserving
  the submitted values so the user can correct them in place.
- **Transport / ASCOM error:** same banners as the GET path.

### Reconnect poll (`GET /config/{service}/status`)

- `config.get` **succeeds** → 200 with the refreshed form fragment (HTMX swaps it
  in and the polling stops).
- `config.get` **fails** (driver still down mid-reload) → 200 with the same
  reconnecting fragment so HTMX keeps polling. The blip is normally well under a
  second; the poll is bounded only by the user leaving the page.

### Restart via Sentinel (`POST /config/{service}/restart`)

Sentinel owns *process* restart (the config-actions plan's service-lifecycle
split); the BFF is just an authorised client of Sentinel's
[Service Restart API](sentinel.md#service-restart-api). Two affordances lead
here, both rendered only when the BFF has a `sentinel` block configured:

- **The recovery hammer**: a config card carries a "Restart via Sentinel"
  button (`hx-post="/config/{service}/restart"`, swapping `#config-card`) in
  its footer — for a wedged or misbehaving driver, independent of any config
  edit. On rp's page the Sentinel-side name is the `rp` convention. On a
  roster-derived device page the name is **derived, not configured**: the
  BFF matches the device's `alpaca_url` port (explicit only — a portless
  URL never matches via the scheme default) against Sentinel's discovered
  services (`GET /api/services` `probe_port` —
  [sentinel.md](sentinel.md#get-apiservices)), guarded to the sentinel
  target's own host (loopback spellings are treated as one host) — Sentinel
  restarts processes on its own box, so a device on another host must never
  grow a button that would bounce an unrelated local service. No match, or
  Sentinel unreachable at render time, degrades to no button.
- **The `restart_required` escalation**: when `config.apply` returns a
  non-empty `restart_required[]`, the restart callout listing those paths
  offers the same restart button inline. rp reaches this on every apply
  (`ApplyDisposition::Restart` — no in-process reload); no driver classifies
  fields this way *today*, so the driver-side path is covered by handler unit
  tests with a stub `ConfigClient`.

Outcome rendering (the Sentinel wire contract is
[sentinel.md §Behavioral contract](sentinel.md#behavioral-contract)):

- **`status:"ok"`** (any `recovery` value) → the driver's process was
  restarted; render the same reconnecting fragment the reload flow uses, which
  polls `GET /config/{service}/status` until the driver serves its config
  again. `recovery:"timeout"` additionally warns that Sentinel could not
  confirm recovery within the budget (the poll may still succeed — the budget
  is Sentinel's, not the driver's).
- **`status:"failed"`** → render an error banner with Sentinel's `detail`
  and a retry button; the form is re-rendered untouched underneath.
- **HTTP 404 / 409 / transport error** → error banner naming Sentinel and the
  reason (unknown service name, not restartable, restart already in flight,
  Sentinel unreachable).
- **No `sentinel` block configured** → the route answers with an error card
  ("no Sentinel configured"); the buttons that would reach it are not rendered.

## Equipment page (`/equipment`)

The roster view of the observatory, per the
[federated-roster design](../plans/archive/config-actions.md#federated-roster-managed-own-vs-foreign-devices).
Its two data sources are joined by device `id`:

- **`GET /api/config`** (rp) — the authoritative device list: every equipment
  entry with its `alpaca_url`, device number, and settings (secrets redacted).
- **`GET /api/equipment`** (rp) — live state: `{ id, connected }` per device
  (the singular mount has no id).

Per device the page renders: name/id, kind, address, a **connected LED**, the
**capability tier**, and Edit / Remove / Configure affordances. The tier comes
from a bounded, concurrent **capability probe** against the device's own Alpaca
server (short per-probe timeout, all devices probed in parallel at render
time):

| Probe result | Tier | Affordance |
|---|---|---|
| `supportedactions` lists `config.get` | **Managed** | "Configure" → `/config/rp:{kind}:{id}` |
| 2xx but no `config.*`; `/setup/v1/{type}/{n}/setup` reachable | **Setup page** | external link to the device's own setup UI |
| 2xx but no `config.*`, no setup page | **Control only** | badge |
| 401/403 | **Auth required** | badge + hint that the BFF holds no credentials for it |
| transport error / timeout | **Unreachable** | badge |

Because `config.*` is self-advertising, any third-party server adopting the
convention auto-upgrades to *Managed* — the probe is the capability detection,
not a hardcoded table.

**Editing the roster edits rp's config.** Add / edit / remove perform a
read-modify-write on the equipment arrays: `GET /api/config` → splice the entry
→ `PUT /api/config`, surfacing the apply outcome (validation errors render
field-level, re-anchored from rp's absolute paths onto the entry form; success
renders the restart callout, since roster changes take effect on the next rp
start). **The list always shows the roster rp is *running***: `GET /api/config`
returns the effective config, so a just-persisted entry appears (or a removed
one disappears) only after rp's next start — the callout names the pending
paths; restart rp from its own config page's
[Restart via Sentinel](#restart-via-sentinel-post-configservicerestart) button
(when a `sentinel` block is configured). An
empty form input means "unset — rp's default applies"; it is never sent as an
empty string (which would fail rp's typed parses, e.g. a humantime
`poll_interval`). The add/edit forms are **schema-generated per
kind**: the field list comes from walking that kind's item subschema inside
`GET /api/config/schema` (the same `FieldModel` walker the config pages use,
entered at the array's item definition), so a new field on, say,
`CameraConfig` appears on the form with **no BFF change**. Composite leaves
(e.g. a device's optional `auth` block) follow the same rule as config pages —
skipped by the walker, preserved on edit, absent on add — and are edited in
rp's config file when needed. The mount is singular: "add" is offered only
when `mount` is `null`, and its routes use the fixed id `mount`.

**Deferred:** per-device **connect/disconnect** buttons — rp's registry is
built once at startup and has no runtime connect/disconnect endpoints yet
(marked *(planned)* in [`rp.md`](rp.md)); the LEDs show live truth and the
roster edits the config, which is what exists today. ASCOM UDP discovery
pre-fill remains low-priority per the plan.

**rp unreachable:** the page renders the same error banner + retry as a config
page whose driver is down; roster mutations are disabled with the banner shown.

## Targets inbox (`/targets`)

The operator surface for rp's [Target Store](rp.md#target-store) — P4 of
[planetarium-target-import.md](../plans/planetarium-target-import.md).
Imports land *paused* (`active: false`) with default goals and no framing
angle; this page is where the operator reviews them: attach or adjust
acquisition goals, set the position angle, then **activate** the target
into the planner's rotation — or **discard** it. Operator-created targets
appear too (the store is one list); the inbox is simply the pending
subset.

### Transport: the BFF's first MCP client

Target CRUD is **MCP-only** on rp by design (rp.md Tenet 8 and
§ Target Store: a browser-facing target UI "would need to be an MCP
client like the orchestrator, not a REST caller"). The page drives rp's
target tools — `list_targets`, `get_target`, `update_target`,
`set_goals`, `delete_target` — through the standard
[`rp-mcp-client`](../decisions/017-standard-mcp-client-construction.md)
crate (ADR-017; the BFF is its fourth consumer), constructed from the
**same required [`rp` block](#configuration)** the REST surfaces use:
the MCP URL is `rp.base_url` + `/mcp`, the credential is `rp.auth`, and
the CA is `rp.ca_cert_path`. No new config keys — one rp target, two
transports, and doctor's existing client-target join over that block
covers both.

**Clients are per-request.** Each page request connects, makes its
burst of tool calls, and drops the client — the BFF never holds a
standing MCP client. This is the planetarium-bridge's own lesson made
policy: an idle rmcp client holds an open POST that stalls rp's
graceful stop (the transport is session-less, ADR-021, so that is the
only reason left). It is also the same philosophy as the config pages'
`Connection: close` — target review is low-frequency; connection reuse
buys nothing.

The three-way `rp-mcp-client` error split maps onto page states:
connect/`Request` failures render the *unavailable* card (below), `Tool`
errors surface as form/field errors (rp is healthy and rejected the
input), and `Malformed` renders a generic error banner.

### The inbox page (`GET /targets`)

Two data fetches back the page, both against the one configured rp: the
MCP `list_targets` call (no `active_only` filter — the page shows both
populations), and REST `GET /api/config` for two joins rp's target rows
don't carry:

- **The filter roster** — the union of every
  `equipment.filter_wheels[].filters` entry, the same union rp validates
  goals against at write time (rp.md § Target Store, Decision 10).
- **Train position-angle defaults** —
  `equipment.optical_trains[].default_position_angle_degrees`
  (rp.md § Position angle), for the PA field's inherit hint.

Behavioral contract:

- **Happy path:** two sections. **Inbox** — rows with `active == false`,
  newest `updated_at` first (review the freshest import first).
  **Active roster** — `active == true`, ordered by display name. Each
  row renders: display name + slug, RA/Dec, the catalog line when
  present (`catalog_ref`, object type, magnitude), the provenance line
  ("added by `created_by` · last touched by `updated_by` at
  `updated_at`" — plus the target's `notes`, which for imports is rp's
  human-readable "Imported via … from … at …" line), the position angle
  (the explicit value, or "inherit"), and a per-goal summary
  ("12 × 5m Ha 1x1"). Affordances: **Review** (→ `/targets/{slug}`),
  **Activate** (pending rows) / **Pause** (active rows) as `hx-post`
  buttons, and **Discard** (pending rows only, `hx-confirm`-guarded).
  Discard is deliberately not offered on active rows — retiring a
  target that may own captured frames should be a Pause
  (`active: false`), per rp.md's `delete_target` guidance; deleting an
  active target is still reachable from its review page, caveat shown.
- **Stale-goal flag:** a goal whose `filter` is not in the roster union
  renders a warning badge ("not in the rig's filter roster") on its
  row, in both the summary and the editor. An empty union flags
  nothing — rp's own permissive rule. The badge is a display-only early
  warning: rp validates at write time, so a stale goal means the roster
  *changed after the goal was written* (a filter wheel removed or its
  filters edited since an import's `default_goals` were stamped); left
  alone it would fail mid-session at capture time. Saving a goal set
  that still carries the stale name fails with rp's own validation
  error — the BFF never re-implements the check, it only surfaces it
  early.
- **Empty states:** no targets at all → an empty-state card pointing at
  the import path (press Align in the planetarium — or `add_target`
  over MCP); an empty pending section with active rows → a short
  "inbox empty" note in that section.
- **rp unavailable:** either fetch failing (the MCP connect/call or the
  REST config read — one rp, one health state) renders an honest card
  with a retry link carrying the failure detail. The target tools are
  ungated (rp.md § Safety → In-Flight Tool Calls: reads and
  target-store writes answer while conditions are unsafe), so the card
  normally means rp is down or unreachable; rp's structured safety
  refusal (`SafetyUnsafe`) only reaches it when an operator's
  `safety.gate` gated the target tools. The card names both causes and
  the detail says which. No stale data is rendered beneath it.
- **Progress is not rendered yet.** `list_targets` reports real
  per-goal `good`/`total` now that rp's on-disk frame scan has landed
  (rp.md § Progress derivation). The goals summary still shows only
  `desired_count`; surfacing the progress columns is a follow-up on
  this page, not a gap in the data.

### The review page (`GET /targets/{slug}`)

An unknown slug renders a "no such target" card. Otherwise the form:

- **Editable:** `display_name` (must not be empty — it seeds slug
  allocation on other paths and stays the operator's label),
  `priority` (integer), `position_angle_degrees` (below), `notes`
  (textarea; an emptied field persists as an empty note — the PA field
  is deliberately the *only* one with explicit-null semantics), and the
  goals editor.
- **Display-only:** coordinates, `catalog_ref` and the denormalized
  catalog fields, the slug, writer identity, and timestamps.
  Coordinates are deliberately not editable here: for a framed import
  they are exactly the center the operator composed in the planetarium,
  and a fat-fingered edit would silently destroy the framing
  (`update_target` accepts them for MCP callers; the inbox renders no
  inputs).
- **Editing claims the target.** Any save stamps
  `updated_by: "operator"`, so a pending import edited here stops being
  upsert-eligible for repeated Aligns of the same spot — a later
  nearby import creates a new pending row instead of overwriting the
  operator's work. That is Decision 3's protection working as designed,
  not a bug to report.

#### The position-angle field

The field must keep "inherit the train default" and "explicit 0°
north-up" distinguishable (the plan's P4 note; rp.md § Position angle):

- **Rendered:** the target's explicit angle when set; **blank** when
  inheriting. The hint line names what blank currently means, from the
  config join: the imaging trains' configured defaults ("blank =
  inherit — main: 254.0°") or "blank = inherit — north-up (0°)" when no
  train carries a default.
- **Submitted:** blank → `update_target` is sent an **explicit `null`**
  (clear back to inherit — idempotent when already inheriting); a
  parseable number → sent as that number, so `"0"` is an explicit
  `0.0`, never collapsed with blank; a non-numeric value → a BFF-side
  field error re-rendering the form with the input preserved. Domain
  errors (out of `[0, 360)`, non-finite) come back from rp's validator
  naming the field and re-render the same way — the BFF parses
  numeric-ness only and never re-implements the domain rule.

#### The goals editor

One row per `AcquisitionGoal`, matching the tool wire shape
(rp.md § Target MCP tools): `filter` (text input with a `<datalist>` of
the roster union — free text stays possible, so a stale name remains
visible and editable, badge attached), `binning` (`AxB` text, e.g.
`1x1`), `exposure_duration` (humantime text, e.g. `5m` or `120s`),
`desired_count` (number). **Add goal** appends a blank row
(`hx-get /targets/goal-row`); each row's **Remove** button swaps the row
away via the same route's empty-body response (`hx-target="closest
…"`, `hx-swap="outerHTML"`) — core htmx, no bespoke JavaScript. Rows
with every field empty are dropped on submit as belt-and-braces. The
BFF passes the strings through — `binning`/`exposure_duration`
validation is rp's (`GoalWire`'s per-field errors), surfaced
field-level on the re-rendered form.

### Saving (`POST /targets/{slug}`)

One submit maps to two tool calls on one per-request session:
`update_target` with the scalar subset (display name, priority, notes,
and the PA per the tri-state mapping), then `set_goals` with the full
goal list (atomic replacement per call). A `Tool` error on either
re-renders the form with the error placed on its field and every
submitted value preserved; when `update_target` succeeded and
`set_goals` failed, the banner says so honestly ("details saved; goals
were not") rather than pretending the whole save rolled back — the
store has no cross-call transaction, and hiding the partial apply would
misreport rp's actual state. Success re-renders the form from a fresh
`get_target` with a "Saved" confirmation.

### Activate / pause / discard

- `POST /targets/{slug}/active` (`active=true|false`) →
  `update_target {active}`. Activating is how a pending target enters
  the planner's candidate set (rp.md § Target MCP tools); pausing
  retires an active one without touching its frames. Success re-renders
  the row (fragment) or the inbox (full page); a tool error renders the
  error banner.
- `POST /targets/{slug}/delete` → `delete_target`. The review page's
  delete button carries rp.md's caveat as help text (frames on disk are
  left orphaned; prefer Pause for targets that have captured frames)
  and an `hx-confirm` guard. Success returns to the inbox.

## Activity stream (`/stream`)

The narrative session view from the chosen mock
([`7-stream-fold.html`](../plans/ui-design/mocks/7-stream-fold.html)):
a single-column **event feed** telling the session's story newest-first, a
sticky **status strip** under the nav, and a **fold panel** (the CSS Grid
`0fr → 1fr` checkbox trick — no JavaScript) holding the equipment LED list.
All live behaviour arrives over one SSE connection driven by the vendored
[htmx-ext-sse] extension: the page declares `hx-ext="sse"
sse-connect="/stream/events"` once, and named `sse-swap` regions receive
server-rendered fragments — no hand-written JavaScript, exactly the pattern the
`test-sse` spike proved.

- **The feed** (`sse-swap="feed"` with `hx-swap="afterbegin"`): every rp event
  envelope renders as one card — severity dot (`*_failed` and
  `safety_changed:unsafe` are bad; `*_complete`/`*_settled` ok; `*_started`
  live; `stream_gap` warn), event title, payload-specific detail line (target
  coordinates, exposure duration, HFR, RMS error, error messages, …),
  monospace timestamp, and the operation duration when `elapsed_ms` is
  present. Unknown event types render a generic card (event name + compact
  payload) so new rp events degrade gracefully rather than vanish.
- **The status strip** (`sse-swap` slots): the current-operation label
  (updated on `*_started` / terminal events), the last guide RMS (updated on
  `guide_settled`/`dither_settled`), and the session-state chip (updated on
  `session_started`/`session_stopped`/`safety_changed`). Slots are updated
  from **each event's own payload alone** — the proxy is stateless, so a slot
  a given event doesn't describe simply keeps its previous content.
- **The fold panel**: the equipment LED list, fetched from `/stream/equipment`
  at render and re-fetched on an htmx timer (`hx-trigger="every 10s"`) — there
  are no device-connectivity events to push yet. The mock's guider graph and
  trend-chart cards need telemetry history rp does not expose; they are
  deferred (see [MVP scope](#mvp-scope)).
- **Initial state**: the page renders the strip from `GET /api/session/status`
  (`idle` / `active` / `interrupted`) and the LED panel from
  `GET /api/equipment`; the feed starts empty and fills from the SSE replay.
- **rp unreachable at page load**: the shell renders with an error banner in
  the hero; the SSE connection keeps retrying (below), so the page heals
  without a manual reload.

### The SSE proxy (`/stream/events`)

The browser never talks to rp (BFF pattern; rp also serves no CORS). The BFF
terminates the browser's `EventSource` and holds its own connection to rp's
`GET /api/events/subscribe`, translating JSON envelopes into HTML fragments:

- **Cursor passthrough.** The proxy forwards the browser's `Last-Event-ID`
  (set automatically by `EventSource` on reconnect) to rp as its
  `last-event-id`; a fresh page (no header) subscribes from cursor `0`, so
  rp's retained history (512 events) replays and populates the feed. The
  BFF keeps **no** stream state of its own — reconnect/replay correctness
  lives in rp, where it is already implemented and tested.
- **Frames.** Each rp envelope becomes up to two BFF frames: `event: feed`
  (the card) and the strip-slot frames it warrants. The **final** frame of
  each envelope group carries `id:` = the envelope's `event_seq`, so the
  browser's cursor only advances past an envelope it has fully received —
  a torn delivery replays that envelope (at-least-once; a duplicated feed
  card in that rare race is preferred over a silently missing one).
- **`stream_gap`** (rp signalling replay loss or a lagging consumer) renders
  as a feed divider card ("events were missed"), with no `id`, mirroring rp.
- **rp connection loss** (initial failure or mid-stream): the proxy pushes a
  status-slot fragment ("rp unreachable — retrying"), then **ends the BFF
  stream**. The browser's `EventSource` auto-reconnects with its cursor, so
  retry/backoff and replay come from the platform + rp rather than BFF state.
- **Keep-alive** every 15 s (axum `KeepAlive`), independent of rp's.
- **Shutdown.** Open SSE responses do not end on axum's graceful-shutdown
  signal (axum #2673 — the hazard the `test-sse` spike pinned), so the proxy
  select!s each stream against a service-wide cancellation token wired to the
  `ServiceRunner` shutdown — the same `sse_shutdown` pattern rp uses. The BFF
  therefore shuts down promptly (and flushes coverage in BDD) even with
  browsers connected.

## Configuration

The BFF has its own small config (it is not an ASCOM device), and its
**source of truth is rp's roster** (ADR-016 decision 9, tightened by #569 /
amendment 6): the config is the listening port and where rp is
(`http://127.0.0.1:11115` — the single-box default), and every config target
comes from the roster at request time — there is no second, hand-maintained
device list. The `rp` target is **required**: an rp-less BFF has no purpose,
so a config without the block (or with `"rp": null`) fails loudly at load.
The retired `drivers` override map fails loudly the same way
(`deny_unknown_fields` — the sentinel `services`-map precedent), with the
deletion in doctor's `config.retired-keys` fix catalog. Every block
(`Config` and each nested target/auth struct) rejects unknown keys at
deserialize, so a typo or a key removed by a schema change fails loudly at
load instead of being silently ignored.

```jsonc
{
  "server": {
    "port": 11120,             // BFF listen port
    "bind_address": "0.0.0.0", // interface to bind (default: all interfaces)
    "tls": null,               // optional { "cert": "...", "key": "..." } — serves HTTPS when set
    "auth": null               // optional { "username": "...", "password_hash": "..." } — HTTP Basic on every route
  },
  // The rp roster is the source of truth; the block is REQUIRED (a config
  // without it fails at load). All fields inside it have defaults. The same
  // block backs every rp transport: REST (config/equipment/SSE), and the
  // targets inbox's MCP client (base_url + /mcp, same auth + CA).
  "rp": {
    "base_url": "http://127.0.0.1:11115",    // rp's base URL
    "auth": null,                            // optional Basic credentials for rp
    "ca_cert_path": null                     // optional PEM CA for a TLS-enabled rp
  },
  // Optional: where Sentinel's dashboard/REST API lives. Absent (the default)
  // means no restart affordances are rendered anywhere.
  "sentinel": {
    "base_url": "http://127.0.0.1:11114",
    "auth": null,            // optional Basic credentials for an auth-enabled dashboard
    "ca_cert_path": null     // optional PEM CA for a TLS-enabled dashboard
  }
}
```

The `server` block is the shared `ServerConfig` from
`crates/rusty-photon-server-config` (see ADR-016): `port`, `bind_address`
(default `0.0.0.0`), and optional `tls`/`auth`. Absent `tls`/`auth` — the
default — means plain, unauthenticated HTTP. When `auth` is set, HTTP Basic
credentials are required on **every** route (`/health` included); when `tls`
is set, the BFF itself serves HTTPS with the named certificate/key (enabling
`auth` without `tls` logs a cleartext-credentials warning). The former `bind`
key was renamed to `bind_address` with this adoption — a config still carrying
`bind` fails loudly at load (`deny_unknown_fields`) — and the default bind
moved from loopback to all interfaces.

The restart button's Sentinel-side name is derived, never configured: `rp`
on rp's own page, and on a device page the discovered service whose
`probe_port` matches the device's `alpaca_url` (see
[Restart via Sentinel](#restart-via-sentinel-post-configservicerestart)) —
there is no per-driver wiring anywhere in this file.

### CLI Arguments

| Argument | Description |
|----------|-------------|
| `-c, --config`     | Path to the BFF configuration file. If omitted, the path resolves to the platform config directory (`~/.config/rusty-photon/ui-htmx.json` on Linux, `%PROGRAMDATA%\rusty-photon\ui-htmx.json` on Windows) and is created with `Config::default()` on first start (binds `0.0.0.0:11120`, rp at `http://127.0.0.1:11115`). An explicit `--config` naming a missing file stays a hard error. |
| `--port`           | BFF listen port (overrides `server.port`). |
| `-l, --log-level`  | Log level: trace, debug, info, warn, error. |
| `--service`        | Hidden: run as a Windows service (passed by the Windows service control manager; no-op on other platforms). |

`ui-htmx doctor [--config <file>] [--json]` diagnoses this service's own
config read-only without starting it — see
[doctor.md §Per-service doctors](doctor.md). Top-level flags cannot be
combined with the subcommand (the mixed form would silently ignore them).

## Security

- **The BFF holds rp's and Sentinel's credentials** in its own config, never
  in the page. It authenticates with HTTP Basic auth and trusts the Rusty
  Photon CA via `rusty-photon-tls` — the same client construction `sentinel`
  uses. Config actions are protected by whatever server-wide
  `rp-auth`/`rusty-photon-tls` the target runs; the BFF is just an authorised client (see
  the plan's Security section). Roster-derived config targets are called
  without credentials (rp redacts per-device auth) — the doctor-minted
  service credential (D6) is the path to authenticated devices.
- **The MCP leg follows ADR-017's credential policy.** The targets inbox's
  `rp-mcp-client` presents `rp.auth` as HTTP Basic **only over verified
  HTTPS** (a configured `ca_cert_path` and an `https` base URL); any other
  combination connects unauthenticated with a loud warning — plaintext
  credentials never travel over cleartext. On the single-box plain-HTTP
  default this matches rp's own unauthenticated default.
- **Secrets are already redacted** by `config.get` (`********`), so they never
  reach the browser; the round-trip sentinel keeps them unchanged on apply.
- **BFF-side TLS/auth is the shared server shape.** `server.tls` serves the UI
  over HTTPS and `server.auth` puts every route (`/health` included) behind
  HTTP Basic auth — the same `rusty-photon-tls`/`rp-auth` stack every other service uses.
  Absent both — the default — the BFF serves **plain unauthenticated HTTP** on
  `0.0.0.0:11120`, so on a shared network either enable `tls` + `auth` or set
  `bind_address` to `127.0.0.1` and reach it via an SSH tunnel. Enabling `auth`
  without `tls` logs a warning: credentials would travel in cleartext. (The
  driver credentials the BFF holds are unaffected — the BFF is a client, and
  each driver still enforces its own `rp-auth`/`rusty-photon-tls`.)

## MVP Scope

### In Scope

- A working configuration page for **any** rostered device's driver,
  generated from its `config.schema`: `GET` renders the current config,
  `POST` applies edits via `config.apply`. One BFF serves the whole roster
  at `/config/rp:{kind}:{id}`.
- Validation surfacing (`status:"invalid"` → field-level errors, values
  preserved), plus BFF-side numeric coercion (schema-bounded) before apply.
- The applying/reconnecting flow (`status:"applying"` → HTMX poll until the driver
  is back).
- Editability tiers (override-pinned, hard read-only, locked/identity) sourced
  from the driver's `config.get`/`config.schema`, with the "unlock to edit"
  escape hatch.
- Driver-unreachable / non-config-driver / unknown-service error states.
- **The `rp` config page** over REST (`RestConfigClient`), with the
  restart-callout apply result.
- **The equipment page**: roster from rp's config joined with live
  `GET /api/equipment` state, capability tiers via probe, roster-derived
  config targets (`rp:{kind}:{id}`), and schema-generated add/edit/remove of
  roster entries via `PUT /api/config`.
- **The activity stream**: the feed + status strip + fold panel from the
  chosen mock, live over the SSE proxy with cursor passthrough, `stream_gap`
  rendering, rp-unreachable self-healing, and the shared-nav night-vision
  toggle.
- **The targets inbox**: pending/active listing with provenance and
  stale-goal flags, the per-target review form (goals editor, tri-state
  position-angle field, activate/pause/discard), all driven through
  `rp-mcp-client` per-request sessions (see
  [Targets inbox](#targets-inbox-targets)).
- The **Restart via Sentinel** affordance (button per config card when a
  `sentinel` block is configured; device pages derive the target service by
  the `probe_port` match) and the restart callout's inline restart button,
  both posting to `/config/{service}/restart` (Phase 4 of the
  config-actions plan).
- **BFF-side TLS + HTTP Basic auth** via the shared `server` block
  (`rusty-photon-tls`/`rp-auth`, wrapping the whole router — see
  [Configuration](#configuration) and [Security](#security)).
- Dark theme reusing the mock CSS tokens; assets embedded via `include_str!`
  (CSS + the HTMX bundle + the SSE extension); no npm, no WASM.
- Plain-axum lifecycle under `rusty-photon-service-lifecycle::ServiceRunner` with
  graceful shutdown (SSE streams end on the shutdown token); prints
  `bound_addr=<host>:<port>` on bind (for BDD port discovery).

### Deferred

- **Roster connect/disconnect buttons** — rp has no runtime
  connect/disconnect endpoints yet (its registry is built once at startup;
  the endpoints are *(planned)* in `rp.md`). The LEDs show live state.
- **ASCOM UDP discovery pre-fill** for the roster (low-priority per the plan;
  manual entry is the primary path).
- **Telemetry charts** — the mock's guider graph and HFR/temp/sky/dew trend
  cards need telemetry history rp does not expose; the fold panel ships with
  the equipment LEDs, and the strip carries the last guide RMS from
  `guide_settled`/`dither_settled` events.
- **Image thumbnails in the feed** — `exposure_complete` links a document id;
  rendering pixels (`GET /api/images/{id}/pixels` ImageBytes → browser image)
  is a follow-up.
- **Composite-field rendering.** The schema walker skips `oneOf`/`anyOf`/`enum`
  subtrees (tagged enums like `star-adventurer`'s `transport`, optional nested
  structs — including a roster entry's optional `auth` block), so those fields
  round-trip read-only via the hidden blob rather than rendering an editable
  discriminated form. A generic `oneOf`/enum renderer (and a dedicated password
  input for redacted-secret leaves) is a follow-up; until then such fields are
  edited in the config file. The array-of-integer-`enum` case is **no longer
  deferred** — it renders as a checkbox group (see
  [Schema-driven rendering](#schema-driven-rendering-fieldmodel)); scalar
  `enum` leaves and `oneOf` forms remain follow-ups.
- **Targets-inbox follow-ups**: per-goal progress columns (rp's
  frame-scan derivation now supplies real `good`/`total`; the page has
  yet to render them); per-target
  `scheduling` editing (a composite with replace-whole-object semantics —
  edited over MCP when needed); `grading` overrides (rp accepts them on
  `add_target`/`update_target`, but this form does not offer them);
  and a dedicated presentation for rp's `SafetyUnsafe` refusal in the
  unavailable card (the detail text carries it today).
- The **LCARS theme** and **i18n**.

## Testing Strategy

Follows [`docs/skills/testing.md`](../skills/testing.md).

### BDD Tests (Cucumber)

`config_page.feature` is the canonical contract for the page behaviour, and —
like every other service — it exercises the **real binaries end to end**. Each
scenario spawns the real `ui-htmx` process, a real `rp` with the driver
rostered as cover calibrator `dsd-fp2`, and a real `dsd-fp2` driver in mock
mode (via `bdd_infra::ServiceHandle`), and drives the BFF over HTTP at the
roster-derived `/config/rp:cover_calibrators:dsd-fp2`, asserting on the HTML
it actually renders. There is no in-process router and no stubbed
`ConfigClient`: the production `ReqwestHttpClient` → `AlpacaConfigClient`
path and the driver's real `config.get` / `config.apply` / in-process reload
all run for real. The entry
point therefore uses `bdd_infra::bdd_main!` (child-process spawning, skipped
under Miri), and both binaries are built with the driver's mock transport (it is
feature-gated):

```
bazel test //services/ui-htmx:bdd
```

**Assertions are DOM-based, and the helpers follow the htmx contract.** The
Then-steps parse each response with [`scraper`] (the Servo html5ever/selectors
stack browsers ship) and assert with CSS selectors — `input[name="…"]`
editability, the `div.banner.applying` reload state, the `#config-card`
`hx-get`/`hx-trigger` poll wiring — rather than `String::contains` substrings
(which mishandle attribute order, boolean attributes, and the value buried in the
hidden `__config` blob). The request helpers drive the BFF the way htmx would:
`submit_form` reads the hidden blobs and enabled controls from the **rendered**
form and POSTs them with the `HX-*` header set (disabled fields are omitted, just
as a browser omits them); the unlock step follows the page's own rendered
`hx-get` link; and the reconnect poll matches the refreshed input's `value`. This
is Layer A of the [UI-testing plan](../plans/archive/ui-testing.md), proving the page's
markup and `hx-*` wiring are correct (obligation P1) without a browser; `scraper`
is a test-only dev-dependency and is never compiled into the shipped binary.

[`scraper`]: https://docs.rs/scraper/

**Byte-equivalence snapshots ride the same scenarios** (Layer B / P2). Selected
Then-steps also capture the response's exact bytes as committed [`insta`] goldens
under `tests/snapshots/` — the server's output is the *cross-OS-comparable*
artifact, since htmx swaps a fragment verbatim, so byte-identical output across
OSes implies identical browser behavior without a browser on every OS. The
driver's OS-assigned `:0` bound port is filtered to `<port>` (the only
run-varying token); the driver-unreachable error card is *not* snapshotted (its
banner carries an OS-specific connection-refused string — the case where the P1
DOM check stands in for P2). Goldens are updated Cargo-locally (`cargo insta
review` / `accept`, then commit) and compared read-only under Bazel
(`INSTA_UPDATE=no`, goldens shipped via the `bdd` target's `data`); a runtime
resolver finds them under both build systems' layouts.

[`insta`]: https://insta.rs/

**Real-browser scenarios are opt-in** (Layer C / P3). `tests/features/browser.feature`
(tagged `@browser`) drives a real headless Firefox via [`thirtyfour`] + geckodriver
to prove the one thing server-output layers cannot: that the vendored
`htmx.min.js` actually loads and executes the declared swaps. They are **gated
behind `UI_BROWSER_TESTS=1`** (an env var, not a cargo feature, so browser flake
never enters the `--all-features` required gate) and run on a single environment —
the P1/P2 server-bytes layers carry the cross-OS guarantee. geckodriver is an
external system tool (`GECKODRIVER_BINARY`, like `OMNISIM_PATH`); teardown quits
the browser before the BFF/driver stop. Run them under Bazel via the standalone
`--config=browser` (it sets `UI_BROWSER_TESTS=1` + `--spawn_strategy=local` and
forwards `FIREFOX_BINARY`/`GECKODRIVER_BINARY` by name, like `OMNISIM_PATH`):

```
FIREFOX_BINARY=/path/to/firefox GECKODRIVER_BINARY=/path/to/geckodriver \
  bazel test --config=browser //services/ui-htmx:bdd
```

This Bazel path is verified green **on Linux only** (plan §9 Tier 0 step 5
go/no-go): the browser layer runs on a single environment by design, so
macOS/Windows browser-under-Bazel is intentionally not pursued — the cross-OS
guarantee rides the P1/P2 server-bytes layers, which do run on every OS under
both build systems. The always-compiled `thirtyfour` dev-dep stays out of the
required gate: with `@browser` filtered out (env unset), the default BDD suite
is green on all three OSes under both Cargo and Bazel. (Under Bazel the run prints
a benign `cargo metadata failed … will use manifest directory as fallback` — the
insta golden-path resolver's expected fallback in the sandboxed build layout; all
snapshot steps still pass.)

An advisory **nightly** workflow ([`ui-browser-nightly.yml`]) runs this suite
against `main` on ubuntu (non-snap Firefox + geckodriver, `UI_BROWSER_TESTS=1`)
and opens-or-updates a tracking issue on failure. It is **not** a required gate —
browser flake never reddens a PR; the per-PR P1/P2 layers carry correctness.

[`ui-browser-nightly.yml`]: ../../.github/workflows/ui-browser-nightly.yml

`browser.feature` carries four scenarios. Two prove htmx executes — a smoke render
(proves `htmx.min.js` loads) and an unlock-click `outerHTML` swap. Two are Tier 0
robustness checks from the [UI-testing plan](../plans/archive/ui-testing.md) §9 that harden
teardown so BDD subprocess coverage is never silently lost (the §5.4 hazard in
[`testing.md`](../skills/testing.md)):

- **Coverage invariant.** Quitting the browser *before* stopping the BFF lets the
  BFF shut down gracefully and run its `atexit` coverage flush; the scenario
  asserts the stop returns well under the 5s SIGKILL grace, plus a
  `COVERAGE_DIR`-gated non-empty `ui-htmx-*.profraw` check under a coverage run.
- **Worst-case orphan reaper.** geckodriver is spawned in **its own process
  group**, so a *simulated* crash (SIGKILL geckodriver, orphaning Firefox) can be
  cleaned up by a kill-the-tree reaper (`killpg` of the group); the scenario
  asserts zero survivors (a `/proc` scan scoped to that group, so it can never
  match a developer's own Firefox) and that a screenshot + page source landed at
  an absolute, chdir-safe path before the reap.

> **Determinism note.** `thirtyfour` hard-requires `serde_json`'s `preserve_order`
> feature, which unifies across the workspace under `--all-features`. Because the
> form is generated by walking a `serde_json::Value` schema and the hidden blob is
> a serialized `Value`, that feature would otherwise reorder the rendered output
> (map iteration order changes). `pages/mod.rs` therefore sorts schema properties
> explicitly and serializes the blob through `canonical_json`, so the output is
> byte-identical regardless of the feature — keeping the P2 snapshots stable across
> Cargo (`--all-features`) and Bazel (where the binary has no dev-deps).

[`thirtyfour`]: https://docs.rs/thirtyfour/

**Test-only `/fixtures/*` routes (the `test-fixtures` feature).** A second
`@browser` feature ([`fixtures.feature`]) drives a `crate::fixtures` route set that
exists **only** when the `test-fixtures` cargo feature is on — it ships nothing in
the real binary, and the module is `#[coverage(off)]` so it never enters the
coverage numbers. These fixtures exercise htmx behaviors the server-bytes layers
(P1/P2) cannot observe: an `hx-swap-oob` swap updating a *second* region (plus the
negative — htmx silently drops an OOB element whose target is absent), an
`HX-Retarget` header moving a **byte-identical** body to a different target (the
body is a plain fragment; the divergence lives entirely in the response header — a
§A tripwire asserts the header, the browser asserts the landing), and an
`HX-Push-Url` header changing the browser location. The BDD suite spawns a binary
built with the feature: cargo `--all-features` provides it; under Bazel the
`:ui-htmx_fixtures` binary (the dsd-fp2 `_mock` pattern) does, so
`bazel test --config=browser` stays green.

[`fixtures.feature`]: ../../services/ui-htmx/tests/features/fixtures.feature

**Test-only `/fixtures/sse*` routes (the `test-sse` feature).** A third `@browser`
feature ([`sse.feature`]) drives a `crate::sse_fixtures` route set gated on the
separate `test-sse` cargo feature (off by default, `#[coverage(off)]`, ships
nothing). It is the streaming spike for the future live-telemetry UI: a fixture
page wires the vendored htmx SSE extension (`htmx-ext-sse@2.2.3`, vendored
byte-for-byte from upstream; the htmx project is Zero-Clause-BSD — htmx 2.0 split
SSE out of core, so the embedded `htmx.min.js` carries none, and the extension is
`include_str!`'d only under this feature) to **one** `sse-connect` EventSource
feeding **two** `sse-swap` regions, and an axum `Sse` endpoint pushes two named
events on a timer then holds the connection open. Two scenarios prove what only a
browser can: that both regions update from the single connection (async
server-pushed DOM updates, which have no server "bytes" for P1/P2 to assert), and
that an open SSE stream — which never closes on the shutdown signal (axum #2673) —
still allows a graceful, coverage-flushing BFF shutdown **when the browser is quit
first**. The latter is the §5.4 coverage hazard with no in-process escape hatch (the
connection is held by the out-of-process browser), so `driver.quit()` must precede
`ServiceHandle::stop()`; the teardown order in `tests/bdd.rs` enforces it. The BDD
binary carries both `test-fixtures` and `test-sse` (cargo `--all-features`; Bazel
`:ui-htmx_fixtures`), the latter pulling the optional `async-stream` dependency.

[`sse.feature`]: ../../services/ui-htmx/tests/features/sse.feature

The driver binds port 0, so the OS assigns a free port atomically (no racy
preselection); the test discovers it from the driver's `bound_addr=` stdout line.
The one scenario that reloads and reconnects first pins that bound port into the
driver's config via a direct `config.apply`, so the in-process reload rebinds the
*same* port and the BFF can reconnect (the override scenario additionally spawns
the driver with `--port` via `ServiceHandle::start_with_args`). Because the form
is now schema-driven, these scenarios also exercise the real `config.schema`
call end to end. Scenarios:

- The config page renders the driver's current configuration.
- A serial-port override is shown read-only with an explanation.
- The `cover_calibrator.unique_id` identity field is read-only by default with an
  "unlock to edit" affordance, and becomes editable when opened with
  `?unlock=cover_calibrator.unique_id` (the read-only-by-default escape hatch).
- A valid change is applied and the page reports the driver is reloading + polls
  `…/status`.
- The reloaded driver's new configuration is served back through the page —
  drives the real `config.apply` → reload → rebind → `config.get` round trip.
- An unchanged submission reports it was saved with no reload (`status:"ok"` —
  the only no-reload path, since the driver classifies *any* changed field as a
  reload).
- An invalid change re-renders the form with the driver's field-level error,
  the submitted value preserved.
- An unreachable driver surfaces an error banner.

**Restart scenarios spawn a real Sentinel too** (`restart.feature`): the same
`bdd_infra::ServiceHandle` pattern starts the workspace's real `sentinel`
binary (dashboard on port 0, `bound_addr=` discovery) with
`SENTINEL_SERVICE_MANAGER_DIR` pointing at a stub service-manager directory
(see [sentinel.md §The test seam](sentinel.md#the-test-seam-sentinel_service_manager_dir))
that lists the driver's unit; sentinel's config dir is seeded with a sibling
`dsd-fp2.json` carrying the driver's real bound port, so discovery derives
the `probe_port` the BFF's restart match resolves against. Scenarios:

- The device config card offers "Restart via Sentinel" when a sentinel block
  is configured (the port match names the discovered service), and clicking
  it restarts the discovered unit (the stub's `restarts.log` records it) and
  swaps in the reconnecting fragment, which then serves the form again (the
  dsd-fp2 driver never actually died — the stub restart is a stand-in — so
  the poll reconnects immediately).
- A failing restart (the stub's `restart-fail` marker) surfaces Sentinel's
  `status:"failed"` detail in an error banner.
- A device whose driver Sentinel has not discovered renders no restart
  button (no port match); rp's own restart against a Sentinel that has not
  discovered rp surfaces the 404 reason.
- With no `sentinel` block configured, the config card renders no restart
  affordance.

**TLS/auth scenarios** (`auth.feature`, `tls.feature`) spawn the BFF with a
generated CA + service certificate (`rusty_photon_tls::test_cert`) and probe `/health` over
`rusty_photon_tls::client::build_reqwest_client`: with `server.auth` configured, valid
Basic credentials answer 200 while wrong or missing ones answer 401 with the
`WWW-Authenticate` challenge; without it, no credentials are needed; with
`server.tls` configured, the BFF answers over HTTPS.

### Phase 5 BDD (`equipment_page.feature`, `rp_config_page.feature`, `stream_page.feature`)

The rp-backed surfaces follow the same real-binaries rule: scenarios spawn the
real `ui-htmx`, a real **`rp`** (via `bdd_infra::rp_harness` —
`RpConfigBuilder` + `start_rp`), and where the roster needs a live device, a
real mock-mode `dsd-fp2` registered in rp's config as a `cover_calibrator`
entry — an all-first-party stack with **no OmniSim dependency**, so these
suites run everywhere the existing one does. Coverage:

- `rp_config_page.feature`: `/config/rp` renders rp's config over REST
  (secrets redacted, `server.port` read-only); an edit persists to rp's config
  file and renders the restart callout listing the changed paths; an invalid
  edit renders the driver-side field error with the file untouched; rp down →
  error banner.
- `equipment_page.feature`: the roster lists rp's devices with live
  connected LEDs (the dsd-fp2-backed entry probes as **Managed** and links to
  `/config/rp:cover_calibrators:{id}`, which renders that device's real
  schema-driven form end to end); add / edit / remove splice rp's config and
  render the restart callout; an entry pointing nowhere shows **Unreachable**;
  no rp configured → the "no rp configured" card.
- `stream_page.feature`: drives `/stream/events` directly over HTTP (SSE is
  server bytes — no browser needed for P1): a session start/stop against rp
  produces `session_started`/`session_stopped` envelopes that arrive as
  rendered feed-card frames with `id:` = the envelope seq; reconnecting with
  `Last-Event-ID` replays only the missed tail; rp down → the
  "rp unreachable" status frame and stream end. The BDD client drops its SSE
  connection before stopping the BFF (testing.md §5.4). Browser-level SSE
  swap behaviour stays proven by the existing `@browser` `sse.feature` spike.

### Targets-inbox BDD (`targets_page.feature`)

Same real-binaries rule, no OmniSim: scenarios spawn the real `ui-htmx`
and a real `rp` (`bdd_infra::rp_harness`), and seed the store through
**rp's own MCP tools** (`bdd_infra`'s `McpTestClient` calling
`add_target` — including the import form with a `source` block, so
provenance rows are the real thing, not fixtures). Target CRUD needs no
live devices; the filter roster and train PA defaults are config-level.
Coverage:

- A pending import renders in the inbox with its provenance line,
  default goals, and "inherit" position angle; an operator-created
  active target renders in the active roster.
- The review form shows a blank PA field with the train-default hint
  for an inheriting target; saving `0` stores an explicit `0.0` and
  saving blank clears an explicit angle back to inherit (both verified
  through `get_target` over MCP — the blank-vs-0 contract end to end);
  a non-numeric PA re-renders with a field error and the input
  preserved; an out-of-range PA surfaces rp's own domain error.
- Editing goals replaces the goal set; a goal naming a filter outside
  the configured roster is flagged in the inbox — seeded by
  restarting rp over the same `data_directory`
  (`RpConfigBuilder::with_data_directory`) with a shrunk
  `filter_wheels[].filters` list, the only way a stale goal can arise
  since rp validates at write time.
- Activate moves a pending target to the active roster; pause moves it
  back; discard removes it (verified in the store over MCP, not just
  the DOM).
- rp down → the unavailable card naming both causes, with a retry link.

Assertions are DOM-based (`scraper`), like every other suite. Snapshot
goldens cover the byte-stable empty-state page only — target rows carry
run-varying store timestamps, so the dynamic pages rely on the DOM layer
(the same reasoning that keeps the driver-unreachable banner out of the
snapshot set). The unavailable-card presentation is a unit-level
concern (a stubbed client error), not a BDD scenario — driving rp's real
safety gate needs an OmniSim SafetyMonitor this suite deliberately
avoids.

### Unit Tests

- `driver_client.rs`: `AlpacaConfigClient` shapes the `PUT .../action` request
  (form fields, device path) and parses the Alpaca envelope — `Value`-as-JSON-
  string extraction for `config.get`/`config.schema`/`config.apply`,
  `ErrorNumber != 0` → error, `ACTION_NOT_IMPLEMENTED` mapping, HTTP-non-2xx →
  transport error. Mocks `HttpClient`. (The wire types are re-exported from
  `rusty_photon_config::actions`, so there is nothing driver-specific to test.)
- `lib.rs`: `AppState::from_config` (builds the rp handle; rejects
  URL-embedded credentials); the roster-derived restart match
  (`resolve_sentinel_service` — port match, same-host guard, no-match and
  sentinel-down degradation, restart POST targeting the matched name); the
  handler renders the "this driver does not expose configuration" banner on
  `ACTION_NOT_IMPLEMENTED` and the "no configured driver" card for an
  unknown `{service}` — error states the end-to-end suite can't produce —
  driven in-process through `AppState::with_client` with a stub
  `ConfigClient`. The `restart_required` escalation banner (no driver emits
  the classification today) and the `recovery:"timeout"` warning are
  likewise driven through stub `ConfigClient` / `SentinelClient`
  implementations.
- `sentinel_client.rs`: `HttpSentinelClient` shapes the restart POST and parses
  each outcome (`ok`+recovery variants, `failed`+detail, 404, 409, transport
  error), and parses `GET /api/services` into `SentinelService` entries
  (`probe_port` present, absent, null). Mocks `HttpClient`.
- `pages`: the schema walker (`$ref` resolution, plain-object recursion,
  `anyOf`/`oneOf` skipping, `FieldKind` inference, the integer-enum-array →
  checkbox-group case incl. object-array skipping); schema-driven form ⇆ Config
  reconstruction (hidden blob + editable overlay by JSON pointer; override-pinned
  and read-only not overlaid; numeric coercion against schema bounds; float
  leaves; multi-value checkbox-group merge incl. empty selection → empty array;
  redacted-secret round-trip); the locked/identity tier (disabled by
  default, editable when `__unlocked`/`?unlock=` names it, pinned still wins, a
  forged `__unlocked` can't unlock a non-locked field).
- `config.rs`: defaults, the required `rp` target (missing/null rejected),
  the retired `drivers` key rejection, and JSON load.
- `io.rs`: `ReqwestHttpClient` connection-refused error path (mirrors sentinel).
- `driver_client.rs` (`RestConfigClient`): REST request shaping, 200-body
  parsing, 400/500 mapping — mocked `HttpClient`.
- `pages/stream.rs`: table-driven `EventEnvelope → Markup` renderers — one case
  per event family from the rp catalog (started/terminal/failed payload
  fields, severity classes, elapsed formatting, unknown-event fallback,
  `stream_gap` divider).
- `sse_proxy.rs`: the incremental SSE frame parser (frames split across
  chunks, CRLF, multi-line `data:`, comment lines, missing `id:`), and the
  proxy against an in-test axum stub serving a canned rp stream (ADR-004's
  escape hatch — streaming is beyond mockall): cursor forwarding, frame `id`
  placement, gap + disconnect translation.
- `probe.rs`: tier classification per probe outcome (mocked responses),
  timeout → Unreachable, 401 → Auth required.
- `pages/targets.rs`: the PA tri-state (blank → explicit `null`,
  number passthrough, non-numeric → field error), goal-row parsing
  (zip in row order, blank-row dropping, string passthrough), the
  stale-goal flag (roster union membership; empty union flags
  nothing), the config join (wheel union + imaging-train defaults) and
  the inherit hint, inbox ordering (pending by `updated_at` desc,
  active by name), and — through a stub `TargetsClient` for the states
  the end-to-end suite can't produce — the unavailable card naming the
  safety gate and the `Malformed` error banner.
- `pages/equipment.rs`: roster join (config ⨝ status by id, mount pairing),
  config surgery (insert/replace/remove per kind incl. the singular mount),
  and the per-kind subschema field generation.

## Module Structure

| Module | Description |
|--------|-------------|
| `config.rs` | `Config`, the shared `ServerConfig` (re-exported from `rusty-photon-server-config`), the required `RpTarget` + optional `SentinelTarget`, defaults + JSON load. |
| `io.rs` | `HttpClient` trait (`#[cfg_attr(test, mockall::automock)]`) + `ReqwestHttpClient` (rusty-photon-tls CA trust + optional Basic auth). |
| `driver_client.rs` | `ConfigClient` trait + `AlpacaConfigClient` (ASCOM action transport) + `RestConfigClient` (rp's plain-REST transport): request shaping, envelope parsing, error mapping. Re-exports the shared wire types from `rusty_photon_config::actions`. |
| `sentinel_client.rs` | `SentinelClient` trait + `HttpSentinelClient`: `POST /api/services/{name}/restart` request shaping + outcome/404/409 parsing, and `GET /api/services` (the `probe_port` listing the restart match resolves against). |
| `rp_client.rs` | The non-config rp surface: `RpApi` trait (`equipment_status`, `session_status`) + its reqwest impl — the seam the equipment page and stream shell render from. |
| `targets_client.rs` | `TargetsClient` trait (mockable seam) + `McpTargetsClient`: per-request `rp-mcp-client` sessions driving rp's target tools (`list_targets` / `get_target` / `update_target` / `set_goals` / `delete_target`), with the `McpCallError` → `TargetsError` mapping the pages render from. |
| `pages/targets.rs` | The targets inbox: pending/active listing (provenance, stale-goal flags, goal summaries), the review form (goals editor + goal-row fragment, PA field with inherit hint), the form parsing (PA tri-state, goal-row zip), its handlers, and the roster/train-default join over rp's config value. |
| `roster.rs` | The roster domain: `EquipKind` (kind ⇄ ASCOM-type mapping), `parse_roster` over rp's config value, the `rp:{kind}:{id}` key codec, and the insert/replace/remove config surgery with duplicate-id/singular-mount guards. |
| `pages/mod.rs` | The schema-driven renderer: `FieldModel` (schema walker + `FieldKind`, incl. the integer-enum-array checkbox group and the array-item subschema entry point), `config_card`/fragment templates, the schema-driven `merge_form` coercion over duplicate-key-preserving form pairs, and the shared `layout` shell (nav tabs + night-vision toggle). |
| `pages/equipment.rs` | The equipment page: roster join, tier badges, add/edit/remove forms, roster mutation via config surgery. |
| `pages/stream.rs` | The activity stream page shell + per-event feed-card and strip-slot fragment renderers (pure `EventEnvelope → Markup` functions). |
| `probe.rs` | The capability probe: bounded concurrent `supportedactions`/setup-page checks → tier. |
| `sse_proxy.rs` | `/stream/events`: rp SSE client (incremental frame parser), envelope→fragment translation, cursor passthrough, shutdown token. |
| `assets.rs` | `include_str!` of `assets/app.css` + `assets/htmx.min.js` + `assets/htmx-ext-sse.js`; asset routes. |
| `lib.rs` | `build_router`, `AppState` (rp handle + Sentinel client + the roster-derived resolve incl. the restart port-match), the `/config/{service}` (+ `/restart`), `/equipment*`, `/stream*` handlers, public exports. |
| `main.rs` | CLI (clap) + tracing init; lifecycle owned by `ServiceRunner` (axum — or `rusty_photon_tls::server::serve_tls` when `server.tls` is set — with the optional `rp_auth` layer, graceful shutdown, SSE shutdown token). |

## References

- Design plan: [`docs/plans/archive/config-actions.md`](../plans/archive/config-actions.md)
- Targets inbox (P4): [`docs/plans/planetarium-target-import.md`](../plans/planetarium-target-import.md); rp-side contract: [`rp.md` § Target Store](rp.md#target-store); MCP client construction: [ADR-017](../decisions/017-standard-mcp-client-construction.md)
- Chosen UI direction + stack: [`docs/plans/ui-design/mocks/README.md`](../plans/ui-design/mocks/README.md)
- Driver config-action protocol (Phase 1): [`dsd-fp2.md`](dsd-fp2.md) "Config Actions"
- HTTP-client / mockall pattern: [`sentinel.md`](sentinel.md)
- Lifecycle: [`docs/skills/service-lifecycle.md`](../skills/service-lifecycle.md) "Plain axum service"

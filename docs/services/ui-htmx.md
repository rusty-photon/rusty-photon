# ui-htmx Service (web BFF)

## Overview

`ui-htmx` is the browser-facing, **server-rendered web UI** over the running
rusty-photon system, in the visual direction of
[`docs/plans/ui-design/mocks/README.md`](../plans/ui-design/mocks/README.md). It
is a **standalone backend-for-frontend (BFF)**: a client of `rp` that holds no
UI logic inside `rp` (`rp.md` tenet 7). The service renders HTML on the server
with [axum] + [Maud] and adds interactivity with [HTMX]; there is no npm, no
WASM, no client-side framework.

It serves two surfaces, one nav:

1. **Activity stream** (`/stream`, also the front page `/`) — the live session
   narrative from the
   [`7-stream-fold.html`](../plans/ui-design/mocks/7-stream-fold.html) mock:
   `rp`'s real-time event stream rendered server-side and pushed to the
   browser over SSE, with the equipment's live connection LEDs in its fold
   panel.
2. **Targets inbox** (`/targets`) — rp's [Target
   Store](rp.md#target-store) as an operator surface: review pending
   (paused) targets — typically planetarium imports
   ([planetarium-bridge.md](planetarium-bridge.md)) — edit their
   acquisition goals and framing position angle, and activate them into
   the planner's rotation or discard them. The BFF's **MCP** surface:
   target CRUD is deliberately MCP-only on rp (rp.md Tenet 8), so this
   page drives rp's target tools through
   [`rp-mcp-client`](../decisions/017-standard-mcp-client-construction.md).

Configuration is not one of them. Every service configures itself on its own
`/setup` page ([driver-setup-pages.md](../plans/driver-setup-pages.md)): the
nav's **Setup** link leads to rp's — the equipment roster and rp's settings,
from which each driver's own pages are linked — and the BFF's own
configuration is edited on its own `/setup` ([Setup page](#setup-page)).

The [`rp` target](#configuration) is **required** — every surface is
rp-backed, so an rp-less BFF has no purpose and a config without the block
fails loudly at load.

**JavaScript (htmx) is required.** The UI does not carry a no-JS fallback: the
targets forms submit via `hx-post` (no `method`/`action`), and the row
affordances are `<button hx-post>` (no `<a href>`), so without htmx loaded the
page renders but is inert. This is a deliberate decision (UI-testing plan §7):
the UI is **optional** — rusty-photon runs fully headless — and the genuine
recovery path is ssh + editing the config file; a whole-app no-JS guarantee is
also incompatible with the real-time stream UI. Direct navigation/refresh still
returns a full styled page (the `HX-Request` full-page-vs-fragment branch is
core htmx, not a no-JS feature). The setup pages are the exception: they are the
shared crate's plain HTML forms, which need no JavaScript.

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
rp clients and the page models (target/tech-agnostic) are extracted into
`ui-core`; it would be premature with a single consumer today.

## Architecture

```
                 ┌─────────────────────────────────────────────┐
   browser ────► │  ui-htmx  (services/ui-htmx)                 │  server-rendered HTML
   (HTMX)        │                                              │
                 │  main.rs ─► lib.rs (build_router, AppState)   │
                 │      │                                        │
                 │      ├─► pages/stream.rs ──► RpApi ───────────┼──► rp  GET /api/equipment
                 │      ├─► sse_proxy.rs ───► reqwest stream ────┼──► rp  GET /api/events/subscribe
                 │      ├─► pages/targets.rs ► TargetsClient ────┼──► rp  /mcp (target tools)
                 │      └─► setup.rs ───────► SetupPages ────────┼──► the BFF's own config file
                 └─────────────────────────────────────────────┘
```

Three thin, independently mockable seams keep the handlers testable without a
live rp (the pattern `sentinel` uses for its Alpaca polling — see
[`sentinel.md`](sentinel.md)):

- **`HttpClient`** (`io.rs`) — `get(url)`. The production impl wraps `reqwest`
  and is built through `rusty_photon_tls::client::build_reqwest_client` so it
  trusts the Rusty Photon CA, with optional HTTP Basic auth. Requests send
  `Connection: close` (no keep-alive pooling): a fresh connection per request
  survives rp restarting between calls, and the reads are low-frequency.
  Mocked with `mockall` for unit tests of the layer above.
- **`RpApi`** (`rp_client.rs`) — `equipment_status()`, the wire mirror of rp's
  `GET /api/equipment` (`rp.md` "REST Endpoints"), which the stream page's LED
  panel renders from.
- **`TargetsClient`** (`targets_client.rs`) — rp's target tools over
  `rp-mcp-client`, one session per request ([Targets inbox](#targets-inbox-targets)).

The SSE proxy holds a raw `reqwest` client of its own (the buffered
`HttpClient` seam cannot stream); its unit tests run it against an in-test axum
stub of rp's stream (ADR-004).

## Routes

| Method | Path | Purpose |
|--------|------|---------|
| `GET`  | `/` | `303 See Other` to `/stream`: the activity stream is the front page. |
| `GET`  | `/stream` | The [activity stream](#activity-stream-stream) page. |
| `GET`  | `/stream/events` | The SSE proxy: rp's event stream rendered as HTML fragments (see [SSE proxy](#the-sse-proxy-streamevents)). |
| `GET`  | `/stream/equipment` | Fold-panel equipment-LED fragment; the panel re-fetches it on an htmx timer. |
| `GET`  | `/targets` | The [targets inbox](#targets-inbox-targets): pending targets awaiting review plus the active roster, from rp's `list_targets` MCP tool joined with `get_target_context`'s filter roster and train position-angle defaults. |
| `GET`  | `/targets/{slug}` | The per-target review form: editable display name, priority, position angle (blank = inherit), notes, and the acquisition-goals editor; coordinates and provenance are display-only. |
| `POST` | `/targets/{slug}` | Save the review form: `update_target` (scalar fields, with the position angle's blank ⇒ explicit-`null` mapping) then `set_goals` (full replacement); rp-side validation errors re-render field-level with values preserved. |
| `POST` | `/targets/{slug}/active` | Activate (`active=true` — accept a pending target into the rotation) or pause (`active=false`) via `update_target`. |
| `POST` | `/targets/{slug}/delete` | Discard the target via `delete_target`. |
| `GET`  | `/targets/goal-row` | Goal-editor fragment: a blank goal row for the "Add goal" affordance, or an empty body for the per-row "Remove" swap. |
| `GET`  | `/setup` | The BFF's own [setup page](#setup-page). |
| `POST` | `/setup` | Save it: accepted only from the page itself (the origin check), validated, persisted; the change takes effect at the next start. |
| `GET`  | `/health` | Liveness; returns `OK`. |
| `GET`  | `/assets/app.css`, `/assets/htmx.min.js`, `/assets/htmx-ext-sse.js` | Embedded static assets (`include_str!`). The SSE extension ([htmx-ext-sse] 2.2.3, vendored) is loaded only by pages that stream. |

[htmx-ext-sse]: https://github.com/bigskysoftware/htmx-extensions/tree/main/src/sse

Every page but the setup page shares the [`layout`] shell, whose top nav
carries the two surface tabs — **Activity** (`/stream`), **Targets**
(`/targets`) — with the active tab highlighted; **Setup**, the link to rp's
setup page; **UI settings**, the BFF's own `/setup`; and the mock's pure-CSS
**night-vision toggle** (a page-level red filter preserving dark adaptation; no
JavaScript).

[`layout`]: ../../services/ui-htmx/src/pages/mod.rs

## Setup page

**rp's page, from the nav.** The **Setup** link is rp's `/setup` at the
address the BFF reads rp by (`rp.base_url`). On the single-box rig that address
is a loopback one (`http://127.0.0.1:11115`), which a browser on the operator's
laptop would follow to itself, so the link keeps rp's scheme and port and, when
rp's host is a loopback address (`127.0.0.1`, `::1`, `localhost`), carries the
host the browser reached the BFF by — the request's `Host` header without its
port. Any other host is used as written. The rule is the shared crate's
`origin::link_for`, which rp's own roster page links its drivers by too
(driver-setup-pages D5). On a doctor-issued certificate the substituted
hostname is a SAN; an IP address is one only when doctor was given
`--extra-san`.

**The BFF's own page.** The BFF's configuration — the listening server and
where rp is — is edited on its own `/setup`, the shared
[`rusty-photon-setup`](../crates/rusty-photon-setup.md) crate's server page
over the BFF's `Config` (`setup.rs`), so the rule "a service's configuration is
edited on that service's `/setup`" holds for the BFF too (driver-setup-pages
D9). It is a plain form with the crate's Security section, origin check and
password rules:

- **Fields.** `server.port`, `server.bind_address`, and `rp.base_url` render
  as inputs; `server.tls` and `rp.auth` are optional blocks the page names as
  edited in the configuration file, the latter's password never reaching the
  page; `server.auth` is edited in the Security section. A `--port` flag pins
  `server.port`: the page shows it pinned and never saves it.
- **Validation.** On top of the config's own parse, `rp.base_url` must be an
  `http` or `https` URL carrying no credentials (`user:pass@…` — they would
  reach error messages and logs; the credential belongs in `rp.auth`), the rule
  the BFF's start holds it to.
- **rp's credential is bound to rp's address and CA.** The BFF presents
  the credential to whatever server the file names, at the address it names
  and verified by the CA it names, and a page anyone who reaches the BFF can
  save must not send rp's password to a server typed on it. A save that
  changes `rp.base_url` or `rp.ca_cert_path` therefore asks first (`409`).
  Behind the BFF's own `server.auth`, typing rp's password there keeps
  `rp.auth` for the new address. A BFF without `server.auth` offers no
  password box, since a page anyone can reach must not answer whether a
  guess matches rp's password. Left blank, or without the box, the save
  removes `rp.auth` from the file and the saved page says so, naming the
  field that changed. A removed credential is set again in
  the file, or by `doctor --fix` when the new address names this machine
  ([doctor.md](doctor.md) § Client-target joins). The page does this with
  the crate's `with_bound_credential`
  ([rusty-photon-setup.md](../crates/rusty-photon-setup.md) § Credentials).
- **Saving.** The BFF has no in-process reload, so a save is persisted
  atomically and takes effect at the next start (`ApplyDisposition::Restart`):
  the saved page names the fields that wait for it and says how to restart
  (`systemctl restart rusty-photon-ui-htmx`, the SCM, `brew services`).
  Sentinel's dashboard has a restart button per service for an operator who
  prefers a click.

## Targets inbox (`/targets`)

The operator surface for rp's [Target Store](rp.md#target-store) — P4 of
[planetarium-target-import.md](../plans/archive/planetarium-target-import.md).
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
only reason left). It is also the same philosophy as the REST reads'
`Connection: close` — target review is low-frequency; connection reuse
buys nothing.

The three-way `rp-mcp-client` error split maps onto page states:
connect/`Request` failures render the *unavailable* card (below), `Tool`
errors surface as form/field errors (rp is healthy and rejected the
input), and `Malformed` renders a generic error banner.

### The inbox page (`GET /targets`)

Two data fetches back the page, both MCP calls on the one configured rp:
`list_targets` (no `active_only` filter — the page shows both
populations), and `get_target_context` for two joins rp's target rows
don't carry (rp.md § Target MCP tools):

- **The filter roster** — each name in every
  `equipment.filter_wheels[].filters`, the same list rp validates goals
  against at write time (rp.md § Target Store, Decision 10).
- **Train position-angle defaults** — each imaging train's
  `default_position_angle_degrees` (rp.md § Position angle), for the PA
  field's inherit hint.

Both are rp's to state: the BFF reads them as rp runs them, and never
reads rp's configuration.

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
- **rp unavailable:** either fetch failing (`list_targets` or
  `get_target_context` — one rp, one health state) renders an honest card
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
  `guide_settled`/`dither_settled`), and the safety chip (updated on
  `safety_changed`; it reads "unknown" until the first transition after
  page load — rp keeps no session, so there is no state to fetch at
  render). Slots are updated
  from **each event's own payload alone** — the proxy is stateless, so a slot
  a given event doesn't describe simply keeps its previous content.
- **The fold panel**: the equipment LED list, fetched from `/stream/equipment`
  at render and re-fetched on an htmx timer (`hx-trigger="every 10s"`) — there
  are no device-connectivity events to push yet. The mock's guider graph and
  trend-chart cards need telemetry history rp does not expose; they are
  deferred (see [MVP scope](#mvp-scope)).
- **Initial state**: the page renders the LED panel from
  `GET /api/equipment` (which is also how it learns whether rp is
  reachable); the strip's slots start empty and the feed fills from the
  SSE replay.
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

The BFF has its own small config (it is not an ASCOM device), edited on its
[setup page](#setup-page) or in the file: the listening server and where rp is
(`http://127.0.0.1:11115` — the single-box default). The `rp` target is
**required**: an rp-less BFF has no purpose, so a config without the block (or
with `"rp": null`) fails loudly at load. The retired `drivers` override map
(rp's equipment roster is the only device list) fails loudly the same way
(`deny_unknown_fields` — the sentinel `services`-map precedent). The retired
`sentinel` block (the BFF restarts nothing since its configuration pages
retired) is accepted and ignored instead, with a warning at load: doctor wrote
it into the installs it wired, and an upgrade runs no doctor, so refusing it
would leave an upgraded web UI down. It never reaches the setup page, and the
page's next save drops it. Doctor's `config.retired-keys` fix deletes both.
Every block
(`Config` and each nested target/auth struct) rejects unknown keys at
deserialize, so a typo or a key removed by a schema change fails loudly at load
instead of being silently ignored.

```jsonc
{
  "server": {
    "port": 11120,             // BFF listen port
    "bind_address": "0.0.0.0", // interface to bind (default: all interfaces)
    "tls": null,               // optional { "cert": "...", "key": "..." } — serves HTTPS when set
    "auth": null               // optional { "username": "...", "password_hash": "..." } — HTTP Basic on every route
  },
  // REQUIRED (a config without it fails at load); every field inside has a
  // default. The same block backs every rp transport — REST (equipment, SSE)
  // and the targets inbox's MCP client (base_url + /mcp, same auth + CA) —
  // and the nav's link to rp's setup page (base_url + /setup).
  "rp": {
    "base_url": "http://127.0.0.1:11115",    // rp's base URL
    "auth": null,                            // optional Basic credentials for rp
    "ca_cert_path": null                     // optional PEM CA for a TLS-enabled rp
  }
}
```

The `server` block is the shared `ServerConfig` from
`crates/rusty-photon-server-config` (see ADR-016): `port`, `bind_address`
(default `0.0.0.0`), and optional `tls`/`auth`. Absent `tls`/`auth` — the
default — means plain, unauthenticated HTTP. When `auth` is set, HTTP Basic
credentials are required on **every** route (`/health` and `/setup` included);
when `tls` is set, the BFF itself serves HTTPS with the named certificate/key
(enabling `auth` without `tls` logs a cleartext-credentials warning).

### CLI Arguments

| Argument | Description |
|----------|-------------|
| `-c, --config`     | Path to the BFF configuration file. If omitted, the path resolves to the platform config directory (`~/.config/rusty-photon/ui-htmx.json` on Linux, `%PROGRAMDATA%\rusty-photon\ui-htmx.json` on Windows) and is created with `Config::default()` on first start (binds `0.0.0.0:11120`, rp at `http://127.0.0.1:11115`). An explicit `--config` naming a missing file stays a hard error. |
| `--port`           | BFF listen port (overrides `server.port`; the setup page shows the field pinned and never saves it). |
| `-l, --log-level`  | Log level: trace, debug, info, warn, error. |
| `--service`        | Hidden: run as a Windows service (passed by the Windows service control manager; no-op on other platforms). |

`ui-htmx doctor [--config <file>] [--json]` diagnoses this service's own
config read-only without starting it — see
[doctor.md §Per-service doctors](doctor.md). Top-level flags cannot be
combined with the subcommand (the mixed form would silently ignore them).

## Security

- **The BFF holds rp's credential** in its own config, never in a page. It
  authenticates with HTTP Basic auth and trusts the Rusty Photon CA via
  `rusty-photon-tls` — the same client construction `sentinel` uses. Its setup
  page draws `rp.auth` as edited in the file, so the password never reaches the
  browser.
- **The MCP leg follows ADR-017's credential policy.** The targets inbox's
  `rp-mcp-client` presents `rp.auth` as HTTP Basic **only over verified
  HTTPS** (a configured `ca_cert_path` and an `https` base URL); any other
  combination connects unauthenticated with a loud warning — plaintext
  credentials never travel over cleartext. On the single-box plain-HTTP
  default this matches rp's own unauthenticated default.
- **BFF-side TLS/auth is the shared server shape.** `server.tls` serves the UI
  over HTTPS and `server.auth` puts every route (`/health` included) behind
  HTTP Basic auth — the same `rusty-photon-tls`/`rp-auth` stack every other service uses.
  Absent both — the default — the BFF serves **plain unauthenticated HTTP** on
  `0.0.0.0:11120`, so on a shared network either enable `tls` + `auth` or set
  `bind_address` to `127.0.0.1` and reach it via an SSH tunnel. Enabling `auth`
  without `tls` logs a warning: credentials would travel in cleartext.
- **The setup page refuses a cross-site save.** Its `POST` is accepted only
  from the page itself (the shared crate's origin check), so a page on another
  site cannot write the BFF's config through a browser holding its credential.

## Scope

### In Scope

- **The activity stream**: the feed + status strip + fold panel from the
  chosen mock, live over the SSE proxy with cursor passthrough, `stream_gap`
  rendering, rp-unreachable self-healing, and the shared-nav night-vision
  toggle.
- **The targets inbox**: pending/active listing with provenance and
  stale-goal flags, the per-target review form (goals editor, tri-state
  position-angle field, activate/pause/discard), all driven through
  `rp-mcp-client` per-request sessions (see
  [Targets inbox](#targets-inbox-targets)).
- **The navigation's link to rp's setup page** with the loopback host rule,
  and **the BFF's own setup page** (see [Setup page](#setup-page)).
- **BFF-side TLS + HTTP Basic auth** via the shared `server` block
  (`rusty-photon-tls`/`rp-auth`, wrapping the whole router — see
  [Configuration](#configuration) and [Security](#security)).
- Dark theme reusing the mock CSS tokens; assets embedded via `include_str!`
  (CSS + the HTMX bundle + the SSE extension); no npm, no WASM.
- Plain-axum lifecycle under `rusty-photon-service-lifecycle::ServiceRunner` with
  graceful shutdown (SSE streams end on the shutdown token); prints
  `bound_addr=<host>:<port>` on bind (for BDD port discovery).

### Deferred

- **Telemetry charts** — the mock's guider graph and HFR/temp/sky/dew trend
  cards need telemetry history rp does not expose; the fold panel ships with
  the equipment LEDs, and the strip carries the last guide RMS from
  `guide_settled`/`dither_settled` events.
- **Image thumbnails in the feed** — `exposure_complete` links a document id;
  rendering pixels (`GET /api/images/{id}/pixels` ImageBytes → browser image)
  is a follow-up.
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

Like every other service, the BDD suite exercises the **real binaries end to
end**. Each scenario spawns the real `ui-htmx` process and, where it reads one,
a real **`rp`** (via `bdd_infra::rp_harness` — `RpConfigBuilder` + the
launcher; `bdd_infra::ServiceHandle` for both), and drives the BFF over HTTP,
asserting on the HTML it actually renders. There is no in-process router and no
stubbed client: the production REST, SSE and MCP clients reach rp for real, an
all-first-party stack with **no OmniSim dependency**. The entry point therefore
uses `bdd_infra::bdd_main!` (child-process spawning, skipped under Miri):

```
bazel test //services/ui-htmx:bdd
```

**Assertions are DOM-based, and the helpers follow the htmx contract.** The
Then-steps parse each response with [`scraper`] (the Servo html5ever/selectors
stack browsers ship) and assert with CSS selectors — the stream page's
`sse-connect`/`sse-swap` wiring, the fold panel's `hx-get`/`hx-trigger` poll,
a target row's affordances — rather than `String::contains` substrings (which
mishandle attribute order and boolean attributes). The request helpers drive the
BFF the way htmx would: `submit_rendered_form` reads the enabled controls from
the **rendered** form and POSTs them to its own `hx-post` URL with the `HX-*`
header set (disabled fields are omitted, just as a browser omits them), and a
row button is followed by its own rendered `hx-post` URL. This is Layer A of the [UI-testing plan](../plans/archive/ui-testing.md), proving the page's
markup and `hx-*` wiring are correct (obligation P1) without a browser; `scraper`
is a test-only dev-dependency and is never compiled into the shipped binary.

[`scraper`]: https://docs.rs/scraper/

**Byte-equivalence snapshots ride the same scenarios** (Layer B / P2). Selected
Then-steps also capture the response's exact bytes as committed [`insta`] goldens
under `tests/snapshots/` — the server's output is the *cross-OS-comparable*
artifact, since htmx swaps a fragment verbatim, so byte-identical output across
OSes implies identical browser behavior without a browser on every OS. rp's
OS-assigned `:0` bound port, which the navigation's link to rp's setup page
carries, is filtered to `<port>` (the only run-varying token); the rp-unreachable
card is *not* snapshotted (its detail carries an OS-specific connection-refused
string — the case where the P1 DOM check stands in for P2). Goldens are updated Cargo-locally (`cargo insta
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
external system tool (`GECKODRIVER_BINARY`, like `OMNISIM_PATH`), 0.34 or later:
the harness starts it with `--port 0` and reads back the port it announces on
stdout, so no other process's geckodriver can stand in for it; teardown quits
the browser before the BFF and rp stop. Run them under Bazel via the standalone
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

`browser.feature` carries four scenarios, all on the activity stream page. Two
prove htmx executes — a smoke render (proves `htmx.min.js` loads) and an rp event
landing in the feed through the SSE extension's `sse-swap` (a safety monitor
flipped unsafe on rp; rp replays its history to a fresh subscriber, so the card
arrives whether the flip lands before or after the browser connects). Two are Tier 0
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
> feature, which unifies across the workspace under `--all-features`, and changes a
> `serde_json::Value` map's iteration order. The stream page's payload dumps go
> through `canonical_json`, so the output is byte-identical regardless of the
> feature — keeping the P2 snapshots stable across Cargo (`--all-features`) and
> Bazel (where the binary has no dev-deps).

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
`:ui-htmx_fixtures` binary (the drivers' `_mock` pattern) does, so
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

**TLS/auth scenarios** (`auth.feature`, `tls.feature`) spawn the BFF with a
generated CA + service certificate (`rusty_photon_tls::test_cert`) and probe `/health` over
`rusty_photon_tls::client::build_reqwest_client`: with `server.auth` configured, valid
Basic credentials answer 200 while wrong or missing ones answer 401 with the
`WWW-Authenticate` challenge; without it, no credentials are needed; with
`server.tls` configured, the BFF answers over HTTPS.

**Setup page scenarios** (`setup_pages.feature`) spawn the BFF on a temp config
file, its `rp` block pointing at an address nothing answers on (the page reads no
rp), and drive the page through the shared `bdd_infra::setup_pages_steps!`: the
page shows rp's address; a changed address is saved and waits for the restart;
an address carrying credentials is refused with the file untouched; the `rp.auth`
block is named as edited in the file with its password off the page; moving rp
elsewhere or trusting another CA for it asks first, then removes `rp.auth` and the
saved page names it and the field that changed; behind the BFF's own login, rp's
password typed on that confirmation keeps `rp.auth` for the new address, and a
wrong one is refused with the file untouched; behind no login the confirmation
offers no password box; a save that leaves rp's address and CA keeps it; a file
still carrying the retired `sentinel` block starts, and a save drops the block;
and a save posted from another site is refused.

**Activity stream scenarios** (`stream_page.feature`) cover the page's SSE and
fold-panel wiring, the front page answering with the stream, the navigation's
link to rp's setup page carrying the host the browser used, and the proxy:

- `stream_page.feature`: drives `/stream/events` directly over HTTP (SSE is
  server bytes — no browser needed for P1): a safety monitor flipping
  unsafe and back on rp produces `safety_changed` envelopes that arrive
  as rendered feed-card frames with `id:` = the envelope seq;
  reconnecting with `Last-Event-ID` replays only the missed tail; rp down → the
  "rp unreachable" status frame and stream end. The BDD client drops its SSE
  connection before stopping the BFF (testing.md §5.4). Browser-level SSE
  swap behaviour is proven by the `@browser` layer: `browser.feature`'s stream
  scenario and the `sse.feature` spike.

### Targets-inbox BDD (`targets_page.feature`)

Same real-binaries rule, no OmniSim: scenarios spawn the real `ui-htmx`
and a real `rp` (`bdd_infra::rp_harness`), and seed the store through
**rp's own MCP tools** (`bdd_infra`'s `McpTestClient` calling
`add_target` — including the import form with a `source` block, so
provenance rows are the real thing, not fixtures). Target CRUD needs no
live devices; the filter roster and train PA defaults are config-level,
read back through `get_target_context`.
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
(the same reasoning that keeps the rp-unreachable card out of the
snapshot set). The unavailable-card presentation is a unit-level
concern (a stubbed client error), not a BDD scenario — driving rp's real
safety gate needs an OmniSim SafetyMonitor this suite deliberately
avoids.

### Unit Tests

- `config.rs`: defaults, the required `rp` target (missing/null rejected),
  the retired `drivers` key's rejection, the retired `sentinel` block ignored
  and never written back or shown on the page, and JSON load.
- `setup.rs`: the `UiHtmxDriver` rules — rp's address must be an `http`/`https`
  URL without credentials, it is trimmed, and `--port` pins `server.port`.
- `lib.rs`: `AppState::from_config` (rejects URL-embedded credentials, trims
  rp's address); the link to rp's setup page following the browser's host for a
  loopback rp and written as is otherwise; `/` answering `303` to `/stream`;
  `/setup` served once the pages are attached.
- `io.rs`: `ReqwestHttpClient` connection-refused error path (mirrors sentinel).
- `rp_client.rs`: `GET /api/equipment` parsing (every kind, the singular
  mount) and the non-2xx → transport error mapping — mocked `HttpClient`.
- `pages/mod.rs`: the shell's navigation (both tabs, rp's setup link, the UI
  settings link; none on the bare shell).
- `pages/stream.rs`: table-driven `EventEnvelope → Markup` renderers — one case
  per event family from the rp catalog (started/terminal/failed payload
  fields, severity classes, elapsed formatting, unknown-event fallback,
  `stream_gap` divider).
- `sse_proxy.rs`: the incremental SSE frame parser (frames split across
  chunks, CRLF, multi-line `data:`, comment lines, missing `id:`), and the
  proxy against an in-test axum stub serving a canned rp stream (ADR-004's
  escape hatch — streaming is beyond mockall): cursor forwarding, frame `id`
  placement, gap + disconnect translation.
- `pages/targets.rs`: the PA tri-state (blank → explicit `null`,
  number passthrough, non-numeric → field error), goal-row parsing
  (zip in row order, blank-row dropping, string passthrough), the
  stale-goal flag (roster membership; an empty roster flags nothing),
  the `get_target_context` join (the filter roster and the imaging trains'
  defaults) and the inherit hint, inbox ordering (pending by `updated_at`
  desc, active by name), and — through a stub `TargetsClient` for the states
  the end-to-end suite can't produce — the unavailable card naming the
  safety gate, a failed context read, and the `Malformed` error banner.
- `targets_client.rs`: the MCP URL and credential mapping, the three-way error
  split, and the unwired test default.

## Module Structure

| Module | Description |
|--------|-------------|
| `config.rs` | `Config`, the shared `ServerConfig` (re-exported from `rusty-photon-server-config`), the required `RpTarget`, `CliOverrides`, defaults + JSON load. |
| `setup.rs` | The BFF's own setup page: `UiHtmxDriver` (the `ConfigurableDriver` the shared crate's page runs on — validation, secrets, the `--port` pin, the `Restart` disposition) and the `/setup` routes. |
| `io.rs` | `HttpClient` trait (`#[cfg_attr(test, mockall::automock)]`) + `ReqwestHttpClient` (rusty-photon-tls CA trust + optional Basic auth). |
| `rp_client.rs` | rp's REST surface the BFF reads: the `RpApi` trait (`equipment_status`) + its reqwest impl — the seam the stream page's LED panel renders from. |
| `targets_client.rs` | `TargetsClient` trait (mockable seam) + `McpTargetsClient`: per-request `rp-mcp-client` sessions driving rp's target tools (`list_targets` / `get_target` / `update_target` / `set_goals` / `delete_target` / `get_target_context`), with the `McpCallError` → `TargetsError` mapping the pages render from. |
| `pages/mod.rs` | The shared `layout` shell: the nav tabs, rp's setup link, the UI settings link and the night-vision toggle. |
| `pages/targets.rs` | The targets inbox: pending/active listing (provenance, stale-goal flags, goal summaries), the review form (goals editor + goal-row fragment, PA field with inherit hint), the form parsing (PA tri-state, goal-row zip), its handlers, and the `get_target_context` join. |
| `pages/stream.rs` | The activity stream page shell + per-event feed-card and strip-slot fragment renderers (pure `EventEnvelope → Markup` functions). |
| `sse_proxy.rs` | `/stream/events`: rp SSE client (incremental frame parser), envelope→fragment translation, cursor passthrough, shutdown token. |
| `assets.rs` | `include_str!` of `assets/app.css` + `assets/htmx.min.js` + `assets/htmx-ext-sse.js`; asset routes. |
| `lib.rs` | `build_router`, `AppState` (the rp state, the setup pages, the SSE shutdown token), `RpState::setup_link`, the `/` redirect, public exports. |
| `main.rs` | CLI (clap) + tracing init; lifecycle owned by `ServiceRunner` (axum — or `rusty_photon_tls::server::serve_tls` when `server.tls` is set — with the optional `rp_auth` layer, graceful shutdown, SSE shutdown token). |

## References

- The BFF's origin: [`docs/plans/archive/config-actions.md`](../plans/archive/config-actions.md)
- Targets inbox (P4): [`docs/plans/archive/planetarium-target-import.md`](../plans/archive/planetarium-target-import.md); rp-side contract: [`rp.md` § Target Store](rp.md#target-store); MCP client construction: [ADR-017](../decisions/017-standard-mcp-client-construction.md)
- Chosen UI direction + stack: [`docs/plans/ui-design/mocks/README.md`](../plans/ui-design/mocks/README.md)
- The setup pages the BFF's own `/setup` and rp's are served by: [`docs/crates/rusty-photon-setup.md`](../crates/rusty-photon-setup.md); why the configuration pages moved there: [`driver-setup-pages.md`](../plans/driver-setup-pages.md)
- HTTP-client / mockall pattern: [`sentinel.md`](sentinel.md)
- Lifecycle: [`docs/skills/service-lifecycle.md`](../skills/service-lifecycle.md) "Plain axum service"

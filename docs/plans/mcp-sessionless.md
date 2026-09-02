# Plan: session-less MCP, and an `rp` that serves tools and keeps hardware safe

## Goal

Move `rp` and every first-party MCP client onto the session-less
2026-07-28 MCP protocol on our own schedule, close the safety gap that
the session model has been hiding, and strip `rp` back to its two
jobs — offer tools, keep the hardware safe — by removing the
orchestrator supervision (the `/invoke` protocol and the session
registry) that only existed because a client with a dead MCP session
could not wait and resume by itself.

At the end of this plan:

- `rp` answers every `/mcp` request statelessly. There is no
  `Mcp-Session-Id`, no `initialize`, no `LocalSessionManager`, and no
  300 s idle keep-alive that kills a client waiting for dusk.
- An unsafe transition cancels every in-flight **actuating** tool body
  — a slew, a capture, a centering loop — through one transport-agnostic
  registry, and answers actuating tools with a machine-readable safety
  error until conditions clear. Read-only and stop-class tools keep
  answering, and a read-only or stop-class body already in flight (a
  `park`, a `stop_guiding`, a long `plate_solve`) runs to completion.
- `rp` has no session, no orchestrator registration, no `/invoke`
  client, no `/api/session/*` routes and no completion callback.
  Cooling and the planner's one piece of runtime state stand on their
  own.
- `session-runner` starts its own runs, waits through unsafe periods
  and `rp` outages in-process, and resumes under its existing
  re-entrancy contract without anyone re-invoking it.

## Background

### What the protocol changed

The 2026-07-28 MCP revision ([changelog](https://modelcontextprotocol.io/specification/2026-07-28/changelog))
removes protocol-level sessions and the `Mcp-Session-Id` header
(SEP-2567), removes the `initialize` handshake — every request carries
its protocol version and client capabilities in `_meta` (SEP-2575) —
replaces the standalone GET stream with `subscriptions/listen`,
removes SSE resumability (`Last-Event-ID`), and requires `Mcp-Method` /
`Mcp-Name` headers on every Streamable HTTP POST (SEP-2243). Cross-call
state is supposed to be an explicit, server-minted handle passed as an
ordinary tool argument. Request-scoped notifications such as
`notifications/progress` still ride the response stream of the request
they belong to.

### Where we stand (verified against rmcp 3.1.4, 2026-09-01)

- The workspace is on rmcp 3.1.4, which implements 2026-07-28. Two
  defaults keep us on the old behaviour: rmcp's `ProtocolVersion::LATEST`
  is still `2025-11-25` (on 3.1.4 and on the SDK's `main`), so
  `rp-mcp-client` still sends `initialize`; and the server's
  `legacy_session_mode` defaults to `true`, so those sessions land in
  the `LocalSessionManager` `rp` wires in (`services/rp/src/lib.rs`,
  `routes.rs`, `safety.rs`).
- The version gate is **per request, not per server**
  (`tower.rs` `is_legacy_request` → `uses_legacy_lifecycle`). Any client
  speaking 2026-07-28 today is served statelessly by a fresh handler
  instance that never touches the session manager — so
  `close_all_mcp_sessions` in `safety.rs` cannot see it.
- Worse, and independent of the protocol: rmcp spawns every request
  handler with a detached `tokio::spawn` (`service.rs`
  `spawn_service_task`) and never aborts it. Closing a session cancels
  the serve loop and the response delivery, not the handler task. It
  hands the handler a `RequestContext.ct` child token and expects the
  handler to observe it. `rp`'s tool bodies never read `ctx.ct`
  (`mcp/progress.rs` is the only consumer of the context, for
  `peer`/`meta`). **Today's "close all MCP sessions, cancelling in-flight
  tool calls" (rp.md § Safety step 2) stops the response, not the slew.**
  The enforcer's direct abort-exposure / stop-guiding / park protect the
  hardware, but a `center_on_target` iteration still running would
  re-slew after the park.
- `rp-mcp-client` pins `reinit_on_expired_session = false` as a "hard
  invariant" (ADR-017 § 4): a dead session is the signal to stop acting.
  Statelessly there is no session to be dead; the contract survives
  only as the consumer's error handling.
- rmcp 3.2.0 (released 2026-08-31) fixes `initialize` routing for legacy
  versions (PR #1228) and adds discovery fallback after session-less
  rejections (PR #1211). It changes nothing about defaults and is safe to
  take first.

### What the session registry does today, and who depends on it

`services/rp/src/session.rs` (`SessionManager`) owns: the
idle/active/interrupted registry persisted to `session.session_state_file`;
`POST /invoke` to the one `type: "orchestrator"` plugin with
`{workflow_id, session_id, mcp_server_url, recovery, config}` (3 attempts,
4xx permanent); re-invocation on the safe transition
(`recovery.reason = "safety_interruption"`) and after an `rp` restart
(`"rp_restart"`, ordered behind the first safety poll); the completion
callback `POST /api/plugins/{workflow_id}/complete`; and the three REST
routes `/api/session/start|stop|status`. It is highly encapsulated —
no module outside `session.rs` reads `SessionState` — but it is the
**only** caller of three things that must survive its removal:

- the cooling controller's `start_cooldown()` (session start),
  `start_warmup()` (every transition to idle) and `recover()` (rp
  restart / safety resume) — `cooling.rs:207-265`;
- the planner's `last_filter_key` (`planner/progress.rs`), the one
  piece of session-scoped runtime state, persisted only through the
  session state file (`persist_progress`, called from
  `mcp/built_in/planner.rs:731`);
- `mcp_server_url` in the invoke body, which all three orchestrator
  plugins trim to `<base>` to post their completion.

Outside `rp`: `session-runner`, `calibrator-flats` and `polar-align`
each expose `/invoke` and post completions; `session-runner`'s
`recovery` field drives `params._recovery.*`, the `_recovery`
reservation, and `ToolCallError::SessionTerminated` → exit-without-
completion; `ui-htmx` renders a status chip from `GET /api/session/status`
and two feed cards from `session_started`/`session_stopped`; `doctor`'s
`dialed_url_field` maps `"orchestrator" => "invoke_url"` for the
`joins.*` checks; `crates/bdd-infra`'s `TestOrchestrator` is the fake
`/invoke` target for `rp`'s own BDD. `sentinel` has **no** dependency.
The two BDD scenarios that pin `rp`'s *outbound* re-invocation are
`session-runner/tests/features/recovery.feature` "An rp restart
re-invokes the engine by itself" and "A safety interruption pauses the
session and rp resumes it once conditions are safe".

Three "session" concepts share the word and must not be confused: the
registry above (removed by this plan); the `session.*` config block
(`data_directory`, `file_naming_pattern`, `directory_pattern` stay —
only `session_state_file` goes); and `equipment::session::DeviceSession`
(Alpaca device slots — untouched).

## Decisions

Made together on 2026-09-01. D1–D5 are the MCP migration proper, D6–D11
the strip, D12 the record, D13 what plugins are once `rp` no longer
supervises anyone.

### D1 — Session-less for everyone

`StreamableHttpServerConfig.legacy_session_mode = false`;
`LocalSessionManager` and the `mcp_sessions` plumbing are deleted.
Pre-2026-07-28 clients still work (rmcp serves them statelessly), they
just never get a session — which nothing in `rp` relies on after D3.
`stateless_protocol_metadata_required` stays `false` (lenient) for now;
tightening it is a one-line follow-up once every first-party client is
on D2. `json_response` stays `true`: a progress notification upgrades
the response to SSE, which rmcp clients handle.

### D2 — First-party clients pin `V_2026_07_28` now

`rp-mcp-client` sets the protocol version explicitly instead of
inheriting rmcp's `LATEST`, so the flip is exercised by our CI on our
schedule rather than arriving through a dependabot bump. `connect()`
no longer performs `initialize`; rmcp bootstraps with `server/discover`,
so "rp unreachable" surfaces at connect through discovery, and the
error text changes — `ConnectError::Connect`'s docs and consumers' log
lines are updated to match.

### D3 — One per-request cancellation registry, honoured by every tool body

`McpHandler` holds an `InFlight` registry. Every tool entry derives a
token from `RequestContext.ct` (so a client disconnect cancels too),
registers it for the call's lifetime **together with the tool's class**
(D5's table), and hands it to the blocking helpers in
`mcp/internals.rs`, which race their poll loops against it
(`do_slew_blocking`, `poll_slewing_until_idle`, `do_park_blocking`,
`do_capture`, `do_move_focuser_blocking`, `do_move_rotator_*`, the
centering and auto-focus loops, guiding start/settle waits). A cancelled
body issues its stop-class counterpart where one exists (abort slew,
abort exposure, halt focuser) and returns a tool error
`cancelled: safety` / `cancelled: client disconnected`.

On the unsafe transition the safety enforcer cancels **the actuating
entries only** — the same set the gate refuses afterwards. A read-only
or stop-class body already in flight runs to completion: cancelling a
`park` or a `stop_guiding` that a client issued a second before the
monitor flipped would undo the very thing the transition is for, and a
long `plate_solve` moves nothing. A client disconnect cancels its own
request whatever the class. This is transport-agnostic and closes the
pre-existing hole, so it lands first and stands alone — which is why the
tool classification lives in slice 1, not slice 2.

### D4 — A safety rejection is a JSON-RPC error with a dedicated `rp` code

While conditions are unsafe, a gated tool call is answered HTTP 200 with
a JSON-RPC error in the implementation-defined range the spec leaves to
servers (`-32000`..`-32019`; `rp` takes `-32010` `SafetyUnsafe`), with
`data: {"reason": "safety", "monitor": "<name>"}`. Every MCP client
library surfaces that as a structured error. `rp-mcp-client` maps it to a
new `McpCallError::SafetyStopped` variant; `McpCallError::Request` keeps
meaning "transport loss or protocol error". The HTTP-level 503 middleware
goes away with it (D5 needs the request body anyway).

### D5 — Gate actuating tools only

The gate moves from the `/mcp` route to the tool dispatch, and applies
to **actuating** tools. Read-only and stop-class tools answer while
unsafe, so a waiting client can observe state and secure hardware.
Classification of the built-in catalog:

| Class | Tools | Under unsafe |
|---|---|---|
| Read-only | every `get_*`, `compute_*`, `list_targets`, `get_target`, `get_target_status`, `resolve_target`, `measure_*`, `detect_stars`, `estimate_background`, `plate_solve`, `validate_plan`, `get_plan_schema`, `get_session_progress`, `get_next_target`, `get_safety_status` (new, D5a) | answers |
| Store-only | `add_target`, `update_target`, `delete_target`, `set_goals`, `record_exposure` | answers |
| Stop-class / protective | `abort_slew`, `stop_guiding`, `pause_guiding`, `calibrator_off`, `close_cover`, `park`, `start_warmup` (D7) | answers |
| Actuating | `slew`, `sync_mount`, `unpark`, `set_tracking`, `move_focuser`, `move_rotator`, `set_filter`, `open_cover`, `calibrator_on`, `capture`, `dither`, `start_guiding`, `resume_guiding`, `auto_focus`, `refocus_train`, `center_on_target`, `start_cooldown` (D7) | `SafetyUnsafe` |

Tools documented in rp.md but not yet implemented take their class when
they land; the slice-2 unit test that every registered tool has a class
makes forgetting impossible. Plugin-provided tools are actuating unless
the provider's registration declares `"gate": "none"` for a tool — the
safe default. `sync_mount`
issues no motion but rewrites the pointing model, and is kept gated
conservatively (open item O3). D5a adds a `get_safety_status` tool
(`overall`, per-monitor state, `since`) so a client that does not
consume SSE can poll; the `safety_changed` event stays the push signal.

### D6 — `rp` stops supervising orchestrators

No `type: "orchestrator"` registration, no `/invoke` client, no session
registry, no session state file, no `/api/session/*` routes, no
`/api/plugins/{workflow_id}/complete`, no `session_started` /
`session_stopped` events. `rp` has **no notion of a session at all**.
What the registry used to signal is re-homed by D7 and D8; what it
used to enforce (one workflow at a time) was never enforced at the tool
level and is not now — the [mount motion gate](../services/rp.md#mount-motion-gate)
and per-device state validation remain the concurrency guards. Runs are
started at the orchestrator (D9). The safe transition lifts the gate and
emits `safety_changed`; it re-invokes nobody. An `rp` restart restores
nothing but config; the planner reads its one tie-break input from the
hardware (D8), so there is nothing to restore.

### D7 — Cooling becomes two tools

`start_cooldown` (actuating) and `start_warmup` (protective) expose
`CoolingController::start_cooldown` / `start_warmup` as MCP tools with
the controller's existing semantics (one pass per ladder camera, a
running task is cancelled first, warm-up ramps and switches off). Both
are idempotent to re-issue. `recover()` is deleted: an `rp` restart never
touches a cooler (tenet 3 — the "operator-started session" carve-out that
justified it no longer has a session to point at). The camera keeps its
last commanded setpoint across an `rp` restart; a workflow that wants the
ladder to keep stepping re-issues `start_cooldown`, which adopts a cooler
already regulating at a configured rung without re-selecting. A safety
interruption still leaves the cooler alone. The shipped workflow
documents gain `start_cooldown` after unpark and `start_warmup` in their
`finally` blocks.

### D8 — The planner reads the filter wheel instead of remembering the last frame

The filter-batching tie-break (rp.md § Decision Logic bullet 4) exists
to avoid a physical filter change, and the wheel's current position is
the ground truth for that. The remembered `last_filter_key` was a proxy
that drifts — a focus run on Luminance after H-alpha frames, a manual
`set_filter`, a wheel that homed on power-up — and the only planner
state that needed persisting. It is deleted, file and all.

- `get_next_target` resolves the imaging train's filter wheel by the
  sole-wheel-in-train rule `set_filter` already uses (`train_id` given:
  that train; absent: the one configured wheel, or none), reads it with
  the same call `get_filter` makes, and passes the current filter name
  into the planner as an optional input. The decision function in
  `planner/decision.rs` stays pure: it receives `Option<&str>` and never
  touches a device.
- A read failure, a disconnected wheel, an ambiguous train or a
  filterless rig all pass `None`: no tie-break preference, one `debug!`
  line, the same outcome the missing diary produced. Filterless rigs
  lose nothing — their goals carry no filter.
- `planner/progress.rs`'s `SessionProgress` (the one-field store),
  `SessionManager::persist_progress` and the `progress` block of the
  session state file go. `record_exposure` keeps its contract of
  returning the target's derived progress; the "record the filter as
  the session's most recent" half of its description is removed.

### D9 — Orchestrators start their own runs and ride through outages in-process

- `session-runner` gains a start endpoint of its own (`POST /runs`,
  body `{workflow, params}`, response `{run_id, session_id}`; `GET
  /runs/{id}`; `POST /runs/{id}/stop`); the exact shape is pinned in
  `session-runner.md` in slice 6. `mcp_server_url` and `events_url` come
  from its config for every run (today they are `/validate`-only).
  `calibrator-flats` and `polar-align` get the same treatment;
  `polar-align` already has a `/status` route to report the outcome,
  `calibrator-flats` gains one.
- Three-way error posture in the engine: a tool error stays a workflow
  error; a `SafetyStopped` error pauses the run — stop trigger
  evaluation, wait on `safety_changed` with `new_state: "safe"` (or
  `get_safety_status` on reconnect), then resume through the re-entrancy
  contract from the persisted blackboard **in-process**; any other
  request-level failure (rp restarting, network) is retried with backoff
  for a configurable window (`rp_outage_grace`, default 10 m), after
  which the run fails and records its outcome. Nothing exits to await a
  re-invocation, because nothing re-invokes.
- `params._recovery` survives as the engine's own signal
  (`{"reason": "safety_interruption" | "rp_outage" | "engine_restart"}`)
  set on every in-process resume, so documents that branch on it keep
  working.
- Completion is local: the run's outcome lives in `session-runner`
  (`GET /runs/{id}`) and in its own events; nothing is posted to `rp`.

### D10 — REST session routes and the completion callback are deleted

`/api/session/start`, `/api/session/stop`, `/api/session/status` and
`/api/plugins/{workflow_id}/complete` are removed with no replacement in
`rp` (there is nothing left to start, stop or complete). `ui-htmx` drops
the session chip and the two session feed cards; run status from
`session-runner` is a follow-up (O2).

### D11 — Config fails loud on the removed surface

A `plugins[]` entry with `type: "orchestrator"` and the
`session.session_state_file` key are rejected at load and by
`PUT /api/config` with a message naming this plan's migration
("orchestrator registrations were removed; start runs at session-runner
— see docs/plans/mcp-sessionless.md"). `doctor` gains a check that
reports the same on an installed config. Silent acceptance would leave an
operator believing `rp` will start their session at dusk.

### D12 — ADR-021 records the new contract

A new ADR, "Session-less MCP and the safety contract", supersedes
ADR-017 § 4 (`reinit_on_expired_session`) and § 6 (the three-way error
split now has a fourth member), states the per-request cancellation
registry as the mechanism behind "cancelling in-flight tool calls",
and records why `rp` no longer supervises orchestrators. ADR-017 gets a
"superseded in part by ADR-021" note; it is not edited otherwise.

### D13 — Two plugin roles remain; orchestrators are not one of them

rp.md § Plugin Types names three roles. After D6 there are two, and
the third is no longer something `rp` registers:

- **Tool providers** add tools to the catalog. A provider runs its own
  MCP server; `rp` discovers its tools at startup and proxies them, so an
  orchestrator sees them beside `slew` and `capture` with no way to tell
  the difference. This is the one extension point for third-party
  capabilities (image-quality classifiers, wavefront tools, alternative
  focus routines) and for first-party capabilities that do not belong in
  `rp`'s process. Today the role is specified but not implemented — the
  config type is recognised (`config/mod.rs`) and nothing dials it.
  Slice 8 implements it.
- **Event plugins** (webhooks, barrier gates) stay exactly as they are.
- **Orchestrators** are MCP clients that a person or a scheduler starts.
  `rp` needs no registration to serve one, and keeps none.

For the two first-party services that are orchestrator-type plugins
today:

- `polar-align` is a person-in-the-loop procedure with its own `/status`
  surface. It becomes a self-starting client (D9) and stays a service.
- `calibrator-flats`' logic also ships as the `session-runner` document
  `calibrator_flats.json`, with the service's BDD suite as the oracle.
  **Both are kept, deliberately**: the pair is the one reasonably simple
  procedure that exists as a Rust orchestrator and as a document, and
  their equivalence is worth the duplicated migration work — it is the
  worked example for anyone deciding which form a new workflow should
  take. The service becomes a self-starting client in slice 6 like
  `polar-align`, and its equivalence row in `session-runner.md`'s BDD
  table stays the contract. It is *not* turned into a tool provider: a
  `take_flats` proxied tool would be a third implementation of the same
  procedure. `calibrator-flats.md`'s "retiring it is a separate
  decision" note is replaced in slice 6 by this decision.

Proxied tools carry D5's gate class from the registration (`"gate":
"none"` opt-out, actuating by default) and take part in D3's
cancellation: a safety stop or client disconnect forwards
`notifications/cancelled` to the provider's in-flight request.

## Open items

Recommendations are stated; none blocks slice 1–3.

- **O1 — `session-runner` self-resume on startup.** With nobody
  re-invoking, a `session-runner` that crashed (or was restarted by
  Sentinel) mid-run only resumes if it resumes itself. Recommendation:
  on startup, resume every run whose blackboard is present, ordered
  behind a reachable `rp` and a safe `get_safety_status`, behind a
  `resume_on_start` config default `true`. The run was operator-started;
  this is the same carve-out `rp`'s restart re-invocation used.
- **O2 — Where run status shows up.** `ui-htmx` loses the `rp` session
  chip in slice 5. A follow-up plan points it at `session-runner`'s
  `/runs` surface (and at `polar-align`'s `/status`). Not in scope here.
- **O3 — Gate edge cases.** `sync_mount` gated (conservative), `park`
  and `close_cover` ungated (protective), `calibrator_off` ungated
  (protective). Adjust in slice 2 if disagreed.
- **O4 — Strict per-request metadata.** Turn on
  `stateless_protocol_metadata_required` once slice 3 has all first-party
  clients on D2 and a rig night has passed. Rejects clients that omit
  the 2026-07-28 `_meta` fields.

## Slices

Each slice is an independently shippable PR and leaves every suite
green. 1–3 are the MCP migration and can ship with no strip at all.
4–7 are the strip and land in this order: **4, then 6, then 5, then 7**
— slice 6 precedes slice 5 because `session-runner` must be able to
start runs before `rp` stops starting them. 8 is the tool-provider role
and needs only 2 (gate classes) and 3 (`rp` as a 2026-07-28 client)
before it.

### Slice 0 — rmcp 3.2.0

Merge the dependabot bump. No code change expected (PR #1228 only
affects `initialize` routing for legacy versions, which slice 3 removes
anyway).

### Slice 1 — cancellation registry (D3)

- `mcp/gate.rs`: the D5 tool classification (read-only, store-only,
  stop-class, actuating) as code, with a unit test that every registered
  tool has a class — no default. Slice 2's gate reuses it.
- `mcp/inflight.rs`: `InFlight { entries: Mutex<HashMap<RequestId, (ToolClass, CancellationToken)>> }`,
  `register(ctx, class) -> Guard`, `cancel_actuating(reason)`.
- Every `#[tool]` entry registers with its class; every blocking helper
  takes the token and races its poll loop against it; cancelled bodies
  issue their stop-class counterpart and return `cancelled: <reason>`.
- `SafetyEnforcer` takes `Arc<InFlight>` instead of
  `Arc<LocalSessionManager>`; `close_all_mcp_sessions` becomes
  `cancel_actuating("safety")`. `LocalSessionManager` stays wired to the
  transport until slice 3.
- `mcp/progress.rs` keeps emitting progress; its module doc is rewritten
  to say why (client feedback), not what it used to work around (the
  session keep-alive).
- Tests: unit tests per helper that a cancelled token stops the loop
  within one tick and issues the stop command (paused-time tests like
  the existing progress ones); BDD `safety.feature`: "An unsafe
  transition cancels an in-flight slew" (a long OmniSim slew, unsafe
  flips mid-slew, the tool call returns `cancelled: safety` within 2 s
  and the mount reports not slewing), "An in-flight park completes
  through an unsafe transition" (a `park` issued just before the flip
  returns success and `AtPark` reads true), and "A client that
  disconnects mid-capture has its exposure aborted".
- rp.md § Safety step 2 rewritten to the registry; § Safety Guardrails
  "Safety override" paragraph likewise.

### Slice 2 — safety error and per-tool gate (D4, D5, D5a)

- `SafetyUnsafe` JSON-RPC error (`-32010`, `data.reason/monitor`)
  returned by the tool dispatch for actuating tools while
  `safety_ok == false`; the 503 middleware removed.
- The gate keys on slice 1's `mcp/gate.rs` classes; plugin-provided
  tools default to actuating with the `"gate": "none"` opt-out on the
  registration.
- `get_safety_status` tool.
- `rp-mcp-client`: `McpCallError::SafetyStopped`; the BDD harness's
  client and every consumer updated to match on it.
- Tests: `safety.feature` "Actuating tools answer SafetyUnsafe while
  conditions are unsafe", "Read-only and stop-class tools answer while
  conditions are unsafe" (table-driven over the classes), gate tests in
  `routes.rs` replaced by dispatch tests; `rp-mcp-client` unit test for
  the mapping.
- rp.md § Safety, § Safety Guardrails, § Tool Catalog (class column),
  § Plugin-Provided Tools (`gate` key); ADR-017 § 6 note.

### Slice 3 — session-less transport (D1, D2, D12)

- `rp-mcp-client`: protocol version pinned to `V_2026_07_28`;
  `reinit_on_expired_session` line and its ADR-017 comment removed;
  `connect()` docs updated for discovery.
- `rp`: `legacy_session_mode = false`; `LocalSessionManager` and
  `mcp_sessions` removed from `AppState`, `SafetyEnforcer`, `lib.rs`;
  `planetarium-bridge`'s server the same.
- Tests: the `initialize`-shaped probes in `routes.rs` and
  `safety_steps.rs` / `mcp_host_allowlist_steps.rs` become 2026-07-28
  `tools/call` probes with `Mcp-Method`/`Mcp-Name` headers and the
  `_meta` version fields; a new BDD scenario "A client idle for longer
  than the old keep-alive still completes its next call" (idle 6 minutes
  in paused-time is not possible end-to-end — use a 30 s idle against a
  server whose old keep-alive would have been 5 s if it still existed;
  pin the absence of `Mcp-Session-Id` on the response instead);
  `mcp/tests.rs` progress comments updated.
- ADR-021 written; ADR-017 annotated; rp.md § MCP Server states the
  protocol posture (2026-07-28 served statelessly; older clients served
  statelessly too; no sessions); `session-runner.md` § Safety Behavior,
  `docs/references/workflow-documents.md` § Safety updated to "the call
  fails with the safety error" (the wait/resume lands in slice 6).

### Slice 4 — cooling tools and the wheel-read tie-break (D7, D8)

Lands the replacements before the removal so workflows can switch.

- `start_cooldown` / `start_warmup` tools (classes per D5);
  `CoolingController::recover` deleted; `SessionManager` keeps calling
  the two remaining hooks until slice 5 so nothing regresses in between.
- `get_next_target` reads the resolved wheel and passes the current
  filter into the pure decision function; `SessionProgress`,
  `SessionManager::persist_progress` and the `progress` block of the
  session state file are deleted; `record_exposure` loses its recording
  half.
- Shipped workflow documents (`deep_sky.json`, `calibrator_flats.json`,
  `sky_flat.json`) call the new tools.
- Tests: `camera_cooling.feature` scenarios driven by the tools instead
  of session start/stop; `planner.feature` "The planner prefers the
  target whose next goal matches the filter in the wheel" (set the
  OmniSim wheel, call `get_next_target`, assert the tie falls the wheel's
  way; then move the wheel and assert it flips) and "A wheel read
  failure leaves the tie-break neutral"; decision-function unit tests
  take the filter as an `Option<&str>` argument; the `startup_recovery`
  last-filter assertions go.
- rp.md § Camera Cooling (three subsections), § Session Persistence
  (the `progress` block), § Dynamic Planner → Decision Logic bullet 4,
  § Built-in Tools (`get_next_target`, `record_exposure`).

### Slice 5 — the strip in `rp` (D6, D10, D11)

- Delete `session.rs` (registry, `Orchestrator`, `invoke_with_retry`,
  state file), `SessionStack`/`build_session_stack`, `set_mcp_base_url`
  and `advertised_base_url` (the allowlist plumbing next to it stays —
  it serves every MCP client), the four REST routes, `AppState.session`,
  `McpHandler::with_session_manager`, `OrchestratorRegistration` and
  friends in `config/mod.rs`, `session_state_file` in `config/session.rs`,
  the two events from the catalog.
- D11 rejections in `validate_config` with the migration message; a
  `doctor` check (`rp.orchestrator-registration-removed`).
- `crates/bdd-infra`: `rp_harness/orchestrator.rs` deleted; `add_plugin`
  keeps serving event and tool-provider registrations;
  `with_session_state_file` deleted.
- `rp` BDD: `session_lifecycle.feature` deleted; `startup_recovery.feature`
  reduced to the progress scenarios; `safety.feature` scenario 1 becomes
  "An unsafe transition cancels in-flight actuating work and the safe
  transition lifts the gate"; `event_delivery.feature`'s session-event scenarios
  re-pointed at `safety_changed` / `exposure_started`; the `session.rs`
  unit tests go with the module; `safety.rs` tests lose the session
  assertions.
- `ui-htmx`: `RpApi::session_status`, the chip and the two feed cards
  removed (with their unit and BDD scenarios); `stream_page.feature`
  scenarios re-pointed at events the harness can still provoke.
- `doctor`: the `"orchestrator"` arm of `dialed_url_field` removed with
  its three tests; the `joins.*` docs.
- Docs: rp.md § Orchestrator Registration, § Orchestrator Invocation
  Protocol, § Orchestration (rewritten to "clients drive rp"), § Session
  Persistence, § REST Endpoints → Session, § Module Structure, the events
  table, the config examples; `workspace.md` services table and
  inter-service section; `doctor.md` § Client-target joins; `ui-htmx.md`
  § Activity stream; the orchestrator conventions in
  `docs/skills/testing.md` and `docs/skills/rig-development.md`.

### Slice 6 — orchestrators start themselves and wait in-process (D9, O1)

Ships **before** slice 5 (keeps `/invoke` alive alongside the new
endpoint until `rp` stops calling it).

- `session-runner`: `POST /runs` + `GET /runs/{id}` + `POST
  /runs/{id}/stop`; `mcp_server_url`/`events_url` from config for every
  run; the engine's three-way posture (`SafetyStopped` → pause and wait
  on `safety_changed`, other request failures → backoff up to
  `rp_outage_grace`, then fail); `RunOutcome::Terminated` and
  `Interrupt::Terminated` retired; `params._recovery` set on in-process
  resume; O1 self-resume on startup.
- `calibrator-flats`, `polar-align`: own start endpoints; completion
  stays in `/status`; `/invoke` kept until slice 7.
- BDD: `recovery.feature` rewritten — "A killed engine resumes on
  restart without repeating recorded frames" (O1), "An rp outage pauses
  the run and it completes against the restarted rp", "A safety
  interruption pauses the run and it resumes by itself once conditions
  are safe"; `deep_sky.feature`'s safety scenario likewise; every "a
  session is started via the REST API" step becomes "a run is started".
  `flat_calibration.feature` and `polar_alignment.feature` in their
  services the same.
- Docs: `session-runner.md` § Architecture, § Re-entrancy Contract, §
  Safety Behavior, § Invocation (→ § Runs), § Configuration, the error
  table; `workflow-documents.md` § How a document runs, § Safety;
  `calibrator-flats.md` and `polar-align.md` § Invocation Protocol;
  `calibrator-flats.md` § Overview's "document port" note restated per
  D13 (kept on purpose as the Rust half of the equivalent pair).

### Slice 7 — remove `/invoke` from the orchestrators

Once slice 5 is on `main`: delete the `/invoke` routes, the
`OrchestratorInvocation`-shaped request types, the completion POST
helpers and their tests in all three plugins; `session-runner`'s
`_recovery` reservation stays (it is still engine-set).

### Slice 8 — tool-provider aggregation (D13)

- `mcp/providers.rs`: at startup `rp` builds one `RpMcpClient` per
  `type: "tool_provider"` registration (`mcp_server_url`, optional
  `auth`/`ca_cert` per ADR-017 — the same credential policy every
  first-party client follows), calls `tools/list`, and merges the result
  into the catalog. A name that collides with a built-in or another
  provider fails startup with both sources named (tenet 2). The
  registration's `requires_tools` is checked against the merged catalog
  at startup, as rp.md already promises.
- Proxying: `tools/call` for a provider tool forwards arguments and
  `_meta` (progress token included) and relays the result verbatim;
  progress notifications from the provider are re-emitted to the caller.
  The call registers in the D3 registry like any built-in; when its
  token fires, `rp` sends `notifications/cancelled` for the provider
  request and returns `cancelled: <reason>` to the caller.
- Gate class per D5 from the registration: absent → actuating (gated
  while unsafe, cancelled by the registry); `"gate": "none"` → ungated
  and never cancelled by a safety stop. The key says nothing about
  whether the tool is read-only, store-only or stop-class — `rp` cannot
  know that about a foreign tool, and the two behaviours above are all
  the class is used for.
- Provider outage: the catalog is built once and stays stable (2026-07-28
  wants `tools/list` deterministic and cacheable; `rp` returns `ttlMs`
  and `cacheScope: "private"` on it). A provider that is down answers
  its tools with a tool error naming the provider; the reconnect
  supervisor (rp.md § Device Session Recovery) gains a provider lane
  that re-dials on the same backoff and emits `equipment_changed`-style
  `provider_changed` events. No re-discovery on reconnect — a provider
  whose tool set changed needs an `rp` restart, which the error message
  says.
- `doctor`: `joins.client-transport` / `joins.client-auth` learn the
  `tool_provider` → `mcp_server_url` field the same way they know
  `event` → `webhook_url`.
- Tests: `crates/bdd-infra` gains a stub provider (an rmcp server
  offering `echo` and a long-running `slow_echo`); BDD
  `tool_providers.feature`: "Provider tools appear in the catalog", "A
  provider tool call is proxied with its result", "A colliding tool name
  fails startup", "A safety stop cancels an in-flight provider tool",
  "A provider outage answers its tools with an error and the catalog is
  unchanged", "A gated provider tool answers SafetyUnsafe while
  unsafe"; unit tests for the merge and the cancellation forwarding.
- Docs: rp.md § Plugin Types (two roles), § Tool Provider Registration,
  § Tool Catalog source 3, § Plugin-Provided Tools; `doctor.md` §
  Client-target joins; `workspace.md` § Orchestrator plugins → "Plugins".

## Verification

Per slice, the rule-4 gate (`bazel build //... && bazel test //...`,
`cargo fmt`, both clippy passes) plus:

- Slice 1: the two new `safety.feature` scenarios under OmniSim; a
  manual check on the rig that an unsafe flip mid-slew stops the mount
  within the poll tick.
- Slice 2: the table-driven gate scenarios; `curl` a `tools/call` for a
  read-only tool while `filemonitor` reports unsafe and see a result,
  then an actuating one and see `-32010`.
- Slice 3: `curl -i` a `tools/call` and confirm no `Mcp-Session-Id`
  header; a `session-runner` run against `rp` with `RUST_LOG=rmcp=debug`
  shows `server/discover` and no `initialize`; the full BDD set including
  `polar-align`, `calibrator-flats`, `planetarium-bridge`, `ui-htmx`.
- Slice 4–7: a full simulated night through `session-runner`'s
  `deep_sky.json` under OmniSim with a scripted unsafe window in the
  middle (the `recovery.feature` scenario), then one real rig night
  before O4.
- Slice 8: the `tool_providers.feature` set under the stub provider; on
  the rig, a `session-runner` document that calls a provider tool and
  an unsafe flip during `slow_echo` showing the provider's request
  cancelled in its own log.

## References

- [MCP 2026-07-28 changelog](https://modelcontextprotocol.io/specification/2026-07-28/changelog)
  and [announcement](https://blog.modelcontextprotocol.io/posts/2026-07-28/).
- [rmcp 3.0 migration discussion #969](https://github.com/modelcontextprotocol/rust-sdk/discussions/969);
  [rmcp releases](https://github.com/modelcontextprotocol/rust-sdk/releases);
  [rust-sdk PR #1228](https://github.com/modelcontextprotocol/rust-sdk/pull/1228).
- rmcp 3.1.4 sources consulted: `transport/streamable_http_server/tower.rs`
  (`legacy_session_mode`, `is_legacy_request`, the stateless POST path,
  `CancelOnDisconnect`), `service.rs` (`spawn_service_task`,
  `RequestContext.ct`), `transport/streamable_http_client.rs`
  (`allow_stateless`, `reinit_on_expired_session`), `model.rs`
  (`ProtocolVersion::LATEST`, `STANDARD_HEADERS`).
- [ADR-017](../decisions/017-standard-mcp-client-construction.md);
  [rp.md](../services/rp.md) § MCP Server, § Safety, § Session
  Persistence, § Orchestration; [session-runner.md](../services/session-runner.md)
  § Re-entrancy Contract, § Safety Behavior, § Invocation;
  [workspace.md](../workspace.md) § Project Tenets (tenet 3).
- The plan that built the machinery this one removes:
  [workflow-dsl.md](archive/workflow-dsl.md) D1/D2, the recovery and
  session-registry sections.

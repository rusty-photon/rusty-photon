# ADR-021: Session-less MCP and the safety contract

## Status

Accepted (2026-09-03). Supersedes [ADR-017](017-standard-mcp-client-construction.md)
§ 4 (`reinit_on_expired_session`) and amends its § 6 (the error split).
Implementation is tracked in
[`docs/plans/mcp-sessionless.md`](../plans/mcp-sessionless.md); this
ADR records the contract slices 1–3 of that plan established, and the
reasoning behind the strip its later slices carry out.

## Context

The MCP 2026-07-28 revision removed protocol-level sessions and the
`Mcp-Session-Id` header (SEP-2567), removed the `initialize` handshake
in favour of per-request `_meta` (SEP-2575), and made `Mcp-Method` /
`Mcp-Name` headers mandatory on every Streamable HTTP POST (SEP-2243).
Cross-call state is meant to be an explicit, server-minted handle
passed as an ordinary tool argument.

`rp`'s safety contract was written against sessions. On an unsafe
transition it "closed all MCP sessions, cancelling in-flight tool
calls", and ADR-017 § 4 pinned `reinit_on_expired_session = false` in
the shared client so a consumer would *observe* the dead session and
stop acting. Two things were wrong with that, independently of the
protocol change:

- rmcp spawns every request handler on a detached task and never
  aborts it. Closing a session cancelled the response delivery, not
  the handler: the slew kept slewing while its caller saw a closed
  stream. The enforcer's own abort-exposure / stop-guiding / park
  protected the hardware, but a `center_on_target` iteration still
  running would re-slew after the park.
- rmcp's version gate is per request, not per server. Any client
  already speaking 2026-07-28 was served statelessly by a handler
  instance the session sweep could not see. The contract held only
  for clients that happened to be on the old revision.

Staying on the legacy lifecycle indefinitely was possible (rmcp still
serves it) but would have left the contract resting on a mechanism
that was both leaky and on its way out.

## Options Considered

### Option 1: Keep legacy sessions and patch the sweep

Keep `legacy_session_mode = true`, and make the sweep also abort the
handler tasks it knows about.

- Pros: smallest change; consumers untouched.
- Cons: still blind to 2026-07-28 clients; keeps the 300 s idle
  keep-alive that long slews and exposures had to be defended against
  with progress notifications; ties the safety story to a transport
  feature the protocol has removed.

### Option 2: Session-less transport, contract rebuilt per call (chosen)

Turn sessions off, and express the safety contract as two per-call
mechanisms that do not care how the caller connected: an in-flight
registry that cancels running bodies, and a per-tool gate that refuses
new calls.

- Pros: the contract holds for every client, old or new; a cancelled
  body actually stops its hardware; the idle keep-alive and its races
  disappear; first-party clients exercise the new revision on our
  schedule rather than through a dependency bump.
- Cons: consumers can no longer infer "safety stopped me" from a dead
  connection; they need the explicit signals below.

### Option 3: A server-minted session handle as a tool argument

Mint a handle at "session start" and require it on every actuating
tool, so `rp` can invalidate it on an unsafe transition.

- Rejected: nothing in `rp` needs cross-call state. The only
  session-scoped runtime state (`last_filter_key`) is replaced by a
  device read (plan, D8), and the orchestrator's own progress lives
  with the orchestrator (D9). A handle would reintroduce the thing the
  protocol removed, with `rp` as its registry.

## Decision

1. **`rp` serves `/mcp` statelessly.** `legacy_session_mode` is off
   and rmcp's `NeverSessionManager` backs the transport. No response
   carries `Mcp-Session-Id`. A 2026-07-28 client sends self-contained
   requests; a client on an older revision has its `initialize`
   answered without a session and is served statelessly from then on.
   `stateless_protocol_metadata_required` stays off until every
   first-party client is on the new revision and a rig night has
   passed (plan, O4). `json_response` stays on: a `tools/call` answers
   as JSON unless the body emits a progress notification first.
2. **First-party clients pin `2026-07-28`.** `rp-mcp-client`
   bootstraps with `server/discover` in rmcp's `Discover` lifecycle
   mode — no `initialize`, no fallback — so "rp unreachable" surfaces
   at connect as a failed discovery. `reinit_on_expired_session` is
   gone from the crate: there is no session to expire. ADR-017 § 4 is
   superseded by this point.
3. **The safety contract is per call.** On an unsafe transition the
   in-flight registry cancels every gated body and every `capture`,
   each of which issues its stop-class command and answers the tool
   error `cancelled: safety`; while conditions stay unsafe the gate
   refuses every gated `tools/call` with the `SafetyUnsafe` JSON-RPC
   error (`-32010`, `data.reason = "safety"`, `data.monitor`), and
   every ungated tool keeps answering. A consumer observes a safety
   stop through those two signals — `rp-mcp-client` renders them as
   `McpCallError::Tool("cancelled: safety")` and
   `McpCallError::SafetyStopped` — never through a dead connection.
   ADR-017 § 6's three-way split is therefore four-way: request
   failure (transport loss, protocol error — `rp` is unreachable or
   unhealthy), safety refusal, tool failure, malformed result.
4. **`rp` stops supervising orchestrators.** With no session to
   terminate and the contract expressed per call, `rp` has no reason
   to own an orchestrator's lifecycle: it cannot make one stop faster
   than the gate already does, and re-invoking one after the safe
   transition duplicates state the orchestrator already persists. An
   orchestrator starts its own runs and waits through an unsafe window
   in-process, on `safety_changed` and `get_safety_status` (plan, D6
   and D9). The REST session routes, the `/invoke` protocol and the
   completion callback go with it (D10, D11). This decision is
   recorded here; the strip lands in the plan's slices 5–7.

## Consequences

- A client that goes away mid-call still cancels its call: rmcp
  cancels the request's token when the HTTP request is dropped before
  its response, and the registry's `cancelled: client disconnected`
  path answers it. The signal is the connection, not a session
  `DELETE`.
- The 300 s idle keep-alive and the races it caused with long slews
  and exposures are gone. The progress emission that defended against
  them stays, for callers that supply a `progressToken`.
- Consumers that connect per request (`ui-htmx`, `planetarium-bridge`)
  keep doing so for the one reason that remains — an idle held client
  keeps a long-lived stream open into `rp` and stalls its graceful
  stop — not because sessions would be dead.
- `session-runner`'s posture on a safety stop — best-effort `finally`,
  persist, exit without completion — is unchanged by this ADR; it is
  triggered by the two signals in decision 3 instead of a dead
  session, and is replaced by an in-process wait when D9 lands.
- ADR-017 gets a "superseded in part by ADR-021" note and is not
  edited otherwise.

## References

- [MCP 2026-07-28 changelog](https://modelcontextprotocol.io/specification/2026-07-28/changelog)
  — SEP-2567 (sessions), SEP-2575 (`initialize`), SEP-2243 (headers)
- [ADR-017](017-standard-mcp-client-construction.md) — the shared
  client crate this ADR amends
- [`docs/plans/mcp-sessionless.md`](../plans/mcp-sessionless.md) — the
  migration plan, decisions D1–D13
- [rp design doc](../services/rp.md) § MCP Server (protocol posture),
  § Safety → In-Flight Tool Calls (the registry and the gate)
- [rmcp](https://crates.io/crates/rmcp) 3.2.0 —
  `StreamableHttpServerConfig::legacy_session_mode`,
  `NeverSessionManager`, `ClientLifecycleMode::Discover`

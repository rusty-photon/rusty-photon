# ADR-017: Standard MCP Client Construction

## Status

Accepted. Superseded in part by
[ADR-021](021-session-less-mcp-and-the-safety-contract.md) (2026-09-03):
§ 4 no longer applies — the transport is session-less, so there is no
session to re-establish — and § 6's split has a fourth member, the
safety refusal. The rest stands.

## Context

`rp` runs the workspace's only MCP server, at `/mcp` on the same listener
as its technical HTTP routes (image bytes, event stream, plugin callbacks,
config). Its TLS and HTTP Basic authentication are
**server-wide**: when `server.tls` / `server.auth` are configured,
`rp_auth::layer` and `serve_tls` wrap the whole router — `/mcp` included.
There is no carve-out that leaves the MCP endpoint open.

Three first-party MCP clients existed before this decision —
`session-runner`, `calibrator-flats`, and `bdd-infra`'s `McpTestClient` —
and all three connected with rmcp's
`StreamableHttpClientTransport::from_uri`, which builds a default HTTP
client: **no way to present a credential, no way to trust the Rusty
Photon CA**. Each also carried its own copy of the result-parsing
convention (rp returns tool results as one JSON text content block).

The consequence: the moment an installation is provisioned by doctor
(ADR-016 decision 10 — the D6 work that mints TLS material and the
observatory credential), every MCP consumer stops being able to reach
`/mcp`, failing with a 401 or a TLS trust error. No BDD scenario
exercised `/mcp` with TLS or auth enabled, so nothing caught this.

The identity model itself is settled and is not revisited here: **one
observatory credential** (username `observatory`), plaintext canonical at
`<config-root>/pki/credential`, hash in each server's `server.auth`,
plaintext + CA path wired by doctor into each client's config
(`docs/services/doctor.md` §Provisioning). Sentinel's `service_auth` /
`ca_cert` pair is the client-side wiring precedent, including its policy
that **credentials only ride verified HTTPS**.

## Options Considered

### Option 1: Fix each client in place

Copy sentinel's CA/auth handling into session-runner, calibrator-flats,
and the test client separately.

- Pros: no new crate; smallest diff per service.
- Cons: triplicates security-sensitive construction that must stay
  identical (credential policy, header shape, trust anchors); the
  result-parsing convention stays triplicated; the next MCP consumer
  (the planned ui-htmx Control surface) becomes a fourth copy.

### Option 2: A shared `rp-mcp-client` crate (chosen)

One crate owns transport construction (CA-pinned reqwest client, Basic
credential, rmcp streamable HTTP), the credential policy, and the
result-parsing convention. Consumers keep their own error taxonomies by
mapping from the crate's two-variant error.

- Pros: security policy exists once; the untested authed-`/mcp` surface
  gets one test story; new consumers are a dependency edge, not a copy.
- Cons: one more workspace crate.

### Option 3: mTLS client certificates instead of Basic

- Rejected: the server side is deliberately `with_no_client_auth()`
  (ADR-002); client identity in this system is HTTP Basic with the D6
  observatory credential. Introducing a second identity mechanism for
  MCP only would fork the identity model.

### Option 4: Home the client in `rusty-photon-tls`

- Rejected: that crate is trust/serving infrastructure and deliberately
  carries no protocol dependencies; adding rmcp would couple every TLS
  consumer to MCP.

## Decision

All first-party MCP clients are built through the new
**`crates/rp-mcp-client`** crate. Its construction rules:

1. **Transport**: rmcp `StreamableHttpClientTransport::with_client` over
   a reqwest client from
   `rusty_photon_tls::client::build_reqwest_client(ca)` — the same
   CA-pinning (platform roots disabled) every REST client uses.
2. **Credential**: HTTP Basic from `rp_auth::config::ClientAuthConfig`,
   sent as a precomputed `Authorization` header via rmcp's
   `custom_headers`. (rmcp's own `auth_header` field is Bearer-only and
   is not used.)
3. **Credentials only ride verified HTTPS** — sentinel's policy,
   verbatim: a configured credential without a configured CA (or on a
   non-HTTPS URL) is **not sent**; the client connects unauthenticated
   and logs a loud warning. Plaintext credentials never travel over
   cleartext or unverified channels.
4. **No transparent session re-establishment**:
   `reinit_on_expired_session` stays `false`. rp terminates MCP sessions
   on safety transitions, and session-runner's pinned contract treats a
   dead session as the signal to stop acting, run `finally` best-effort,
   and exit. A transport that silently re-established sessions would let
   a workflow keep acting through a safety stop. This is a crate
   invariant, not a knob.
5. **One result-parsing convention**: empty content → `null`; exactly one
   JSON text block → the parsed value; anything else (non-JSON text,
   non-text blocks, multiple blocks) → a loud error.
6. **Three-way error split**: request-level failures (transport loss,
   JSON-RPC protocol error — the session is unusable) are distinguished
   from tool failures (`is_error` results) and from malformed results
   (convention violations from a live session). Consumers map these onto
   their own taxonomies (session-runner: `Unavailable` for request
   failures, `SafetyStopped` for the safety refusal and cancellation,
   `Failed` for tool failures and malformed results) without re-deriving
   the classification.

   *Note (2026-09-03, mcp-sessionless slice 2):* rp's safety gate moved
   from the HTTP layer (a `503` on every `/mcp` request, indistinguishable
   from an outage) to the tool dispatch, where a gated tool is refused
   with the `SafetyUnsafe` JSON-RPC error (`-32010`,
   `data.reason = "safety"`, `data.monitor`). The crate maps that one
   code to a fourth variant, `McpCallError::SafetyStopped`, so a
   consumer can tell "rp told me to wait" from "rp is gone"; every
   other JSON-RPC error stays `Request`. The session survives a safety
   refusal and ungated tools keep answering.

Client configuration follows sentinel's field shape — `service_auth:
Option<ClientAuthConfig>` + `ca_cert: Option<String>` — and doctor's
`plan_client_wiring` provisions both (absent-only) for every MCP
consumer, exactly as it does for sentinel's probe client.

## Consequences

- `session-runner`, `calibrator-flats`, and `bdd-infra`'s
  `McpTestClient` are rebuilt on the crate; their public behavior and
  error taxonomies are unchanged.
- `session-runner` and `calibrator-flats` gain `service_auth` /
  `ca_cert` config fields, and doctor wires them during `--fix`.
- rp's BDD suite gains the previously-missing coverage: MCP tool calls
  over TLS with credentials, and rejection without them.
- Future MCP consumers (the planned ui-htmx Control surface) start from
  the crate and inherit the policy.
- An MCP client that must *bypass* the credential policy has no
  supported path — by design.

## References

- [ADR-002](002-tls-for-inter-service-communication.md) — TLS
  architecture, `with_no_client_auth()`
- [ADR-016](016-service-config-ownership-and-doctor.md) — decision 10,
  provisioning ownership
- [doctor design doc](../services/doctor.md) §Provisioning — the
  observatory credential and client wiring
- [session-runner design doc](../services/session-runner.md) — the
  session-termination safety contract
- [rmcp](https://crates.io/crates/rmcp) — `StreamableHttpClientTransport`

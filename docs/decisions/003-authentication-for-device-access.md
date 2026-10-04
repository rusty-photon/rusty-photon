# ADR-003: Authentication for Device Access

## Status

Accepted

## Updates

**2026-04-22** — Upstream `ascom-alpaca-rs` replaced
`Server::into_router() -> axum::Router` with `Server::into_service() ->
AlpacaService`. The code snippets below still illustrate the
integration point; in current code each service wraps the returned
service with `Router::new().fallback_service(server.into_service())`
before calling `rp_auth::layer(router, auth)`. The authentication
decision is unchanged — only the upstream adapter shape changed. See
`services/*/src/lib.rs` for the live pattern.

**2026-10-03** — On the Raspberry Pi 5 field rig the Argon2id verify
costs ~41 ms of CPU per authenticated request, and the middleware ran
it inline on a tokio worker, which froze the service's timers and I/O
for that long on every NINA poll, sentinel probe and rp read (late
pulse-guide ends and late poll ticks in the star-adventurer-gti
driver, issue #1388). The verify now runs on the blocking pool behind
a one-permit gate, and a layer-scoped verification memo answers a
repeat presentation of an already-proved credential in microseconds.
The stored credential, its parameters, the wire format and the config
schema are unchanged — see § [Verification memo and KDF admission
control](#verification-memo-and-kdf-admission-control) and
§ [Rejected: cheaper per-request
verification](#rejected-cheaper-per-request-verification-2026-10-03).
The `rp hash-password` / `rp init-tls` commands quoted below moved to
`doctor auth hash-password` / `doctor --fix` with
[ADR-016](./016-service-config-ownership-and-doctor.md); the text was
corrected in place.

## Context

ADR-002 introduced opt-in TLS for inter-service communication, protecting
traffic confidentiality and integrity. However, TLS alone does not answer
the question "who is allowed to talk to this service?" Any client that
can reach the port — on the local network or over the internet — can
issue equipment commands, read sensor data, and control observatory
sessions without restriction.

The ASCOM Alpaca specification explicitly declares no security mechanisms
(`security: []` in both the Device API and Management API OpenAPI
definitions). The design philosophy is that "Alpaca and network security
are separate things." The standard relies on network isolation (private
observatory LANs, NAT routers) as the primary security model.

This works for a single-user home observatory on a dedicated network,
but breaks down in increasingly common scenarios:

- **Remote observatories** accessed over VPN or the internet
- **Shared club networks** with multiple users and devices
- **Mixed networks** where observatory equipment shares Wi-Fi with other
  household or facility devices

The goal is to add opt-in authentication that:

1. Ensures only authorized users can access Alpaca devices
2. Is easy to configure for hobbyist astronomers
3. Uses a common, straightforward scheme that other manufacturers can
   adopt
4. Works for both local and remote device access
5. Does not require fine-grained scopes or role-based access — just
   "authorized or not"

## Options Considered

### Option 1: HTTP Basic Auth over TLS (Chosen)

The client sends `Authorization: Basic <base64(username:password)>` with
every request (RFC 7617). The server validates the credentials and
returns `401 Unauthorized` with `WWW-Authenticate: Basic realm="..."` on
failure.

**Pros:**
- The only scheme already supported by the ASCOM ecosystem — ASCOM
  Dynamic Clients (NINA, SGPro, Windows Platform) have native
  username/password fields in their Alpaca setup dialogues
- The ASCOM OmniSim reference server implements exactly this: HTTP Basic
  Auth with PBKDF2 password hashing, opt-in, off by default
- Trivial to implement — ~20 lines of axum tower middleware
- Trivial to configure — users understand username/password pairs
- Trivial for other manufacturers — every HTTP framework has built-in
  support
- Adequate security over TLS — credentials encrypted in transit, replay
  and tampering prevented at the transport layer
- Same model used by Home Assistant REST API, Tasmota, OctoPrint, NAS
  devices, and router web UIs

**Cons:**
- Credentials are Base64-encoded (trivially reversible), not encrypted at
  the application layer — requires TLS for security
- No built-in expiration or rotation mechanism
- No per-client revocation — changing the password affects all clients
- Sends credentials with every request (no session/token caching)

### Option 2: API Keys (Custom Header)

The server generates a random string. The client sends it as
`X-Api-Key: <key>` or `Authorization: Bearer <key>`.

**Pros:**
- Simple to implement (~30 lines of middleware)
- No username/password semantics — a single opaque token
- Easy revocation — generate a new key and the old one is invalid
- Per-client keys enable selective revocation
- Established pattern in IoT (OctoPrint, Philips Hue, Home Assistant)

**Cons:**
- Not a standard HTTP auth mechanism — no `WWW-Authenticate` challenge,
  custom header names vary across implementations
- No existing ASCOM Alpaca client supports API key headers — breaking
  change for the ecosystem
- Static credentials — no expiration unless manually rotated
- Requires TLS, same as Basic Auth

### Option 3: HTTP Digest Auth (RFC 7616)

Challenge-response: the server sends a nonce, the client hashes the
credentials with the nonce and sends the hash.

**Pros:**
- Password never sent in cleartext, even without TLS
- Nonce-based replay protection

**Cons:**
- Largely deprecated — NIST/CISA guidance favors Basic Auth over TLS
- Complex implementation (~200 lines, nonce management, qop handling)
- `reqwest` (our HTTP client) does not support Digest natively
- No advantage over Basic Auth when TLS is available
- Not used by any ASCOM Alpaca implementation
- The RFC itself acknowledges: "For those needs, TLS is a more
  appropriate protocol"

### Option 4: Bearer Tokens / JWT (RFC 6750 / RFC 7519)

Signed JSON tokens with expiration, issuer claims, etc.

**Pros:**
- Stateless validation via cryptographic signature
- Built-in expiration (`exp` claim)
- Rich metadata for distributed systems

**Cons:**
- Overkill for "authorized or not" on a single device server
- Signing key management adds complexity
- No revocation without a revocation list (defeating "stateless")
- Token generation requires a login endpoint or pre-generation
- No ASCOM Alpaca client support
- Solves distributed identity problems that don't exist here

### Option 5: Mutual TLS / mTLS (RFC 8705)

Both client and server present X.509 certificates during the TLS
handshake.

**Pros:**
- Strongest authentication — cryptographic client identity
- No credentials in the HTTP layer
- Leverages existing PKI from ADR-002

**Cons:**
- Generating, distributing, and installing client certificates is a
  significant usability barrier for hobbyist astronomers
- No existing ASCOM Alpaca client supports client certificates
- Certificate lifecycle management adds operational burden
- Used in industrial IoT (AWS IoT Core, Azure IoT Hub), not consumer
  or hobbyist device control

### Option 6: OAuth 2.0 (RFC 6749)

Authorization server issues tokens via grant flows.

**Pros:**
- Industry-standard framework
- Built-in token lifecycle management
- Separation of auth server and resource server

**Cons:**
- Requires a separate authorization server — running OAuth on a
  Raspberry Pi for a single-user observatory is unreasonable
- No ASCOM Alpaca client support
- Dramatically overengineered for this use case

### Option 7: HMAC Signing (AWS SigV4 style)

Each request is signed using HMAC-SHA256. The signature covers the HTTP
method, URL, headers, timestamp, and body.

**Pros:**
- Request integrity — tampering detected even beyond TLS
- Replay protection via timestamps
- Secret never transmitted

**Cons:**
- ~400 lines of server code, ~200–300 lines per client language
- Fragile canonicalization — minor differences cause signature mismatches
- Clock synchronization required — problematic for observatory setups
  without NTP
- Redundant with TLS integrity and replay protection
- Prohibitive implementation burden for other manufacturers

## Decision

We chose **Option 1: HTTP Basic Auth over TLS (RFC 7617)**.

The decisive factor is ecosystem compatibility. ASCOM Alpaca clients
already have native username/password fields, and the ASCOM OmniSim
reference server validates this exact approach. Every other option would
require changes to third-party client software that we do not control.

Basic Auth over TLS provides adequate security for this threat model:
credentials are encrypted in transit, the TLS channel prevents replay and
tampering, and the residual risk is credential management — the same
tradeoff accepted by Home Assistant, OctoPrint, and every router web UI.

Like TLS (ADR-002), authentication is **opt-in and off by default**.
Services without an `auth` configuration section run unauthenticated,
preserving backward compatibility.

## Implementation

### Credential Storage

Credentials are stored in each service's configuration file. Passwords
are hashed using Argon2id (the current OWASP recommendation for password
hashing):

```toml
[server.auth]
username = "observatory"
password_hash = "$argon2id$v=19$m=19456,t=2,p=1$..."
```

A CLI command generates the hash from a plaintext password:

```bash
doctor auth hash-password
# Enter password: ********
# Confirm password: ********
# $argon2id$v=19$m=19456,t=2,p=1$...
```

The user pastes the output into their service config. This avoids storing
plaintext passwords in configuration files.

**Why Argon2id over PBKDF2:**
The ASCOM OmniSim uses PBKDF2 (RFC 2898, 1000 iterations). Argon2id
(RFC 9106, winner of the Password Hashing Competition) is the current
OWASP recommendation. It is memory-hard, resisting GPU/ASIC attacks that
PBKDF2 is vulnerable to. The `argon2` crate is pure Rust with no system
dependencies.

### Authentication Middleware

A tower middleware layer validates the `Authorization: Basic` header on
every request. The middleware is added to the axum router extracted from
`ascom-alpaca-rs`'s `Server::into_router()`, the same integration point
used for TLS (ADR-002).

```rust
// Pseudocode — actual implementation in rp-auth crate
async fn auth_middleware(
    State(credentials): State<Credentials>,
    request: Request,
    next: Next,
) -> Response {
    match extract_basic_auth(&request) {
        Some((user, pass)) if credentials.verify(user, pass) => {
            next.run(request).await
        }
        Some(_) => unauthorized_response(),   // 401 — wrong credentials
        None    => unauthorized_response(),   // 401 — missing header
    }
}

fn unauthorized_response() -> Response {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("WWW-Authenticate", "Basic realm=\"Rusty Photon\"")
        .body(Body::empty())
        .unwrap()
}
```

The `WWW-Authenticate` header is required by RFC 7235 and triggers
browser credential prompts (useful for accessing the sentinel dashboard).

### Verification memo and KDF admission control

Argon2id's cost protects the *stored* hash against offline cracking; it
is not an online defence, and paying it on every request is a liability
on a small board. Measured on the field rig (Raspberry Pi 5, release
build): a verify with the default parameters takes 40.9 ms median (p95
54 ms) and allocates 19 MiB; every OWASP-compliant parameter set costs
36–58 ms; pre-allocating the memory saves 2 ms; a keyed BLAKE2b-256 tag
of the credential costs 0.5 µs. Tuning parameters cannot remove the
cost — only moving the KDF off the hot path can. HTTP Basic is
stateless, so without a memo every NINA poll, every sentinel probe,
every rp supervisor read and every MCP call pays it, and because the
middleware ran the verify inline on a tokio worker, the worker that
holds the runtime's I/O and timer driver spent those 41 ms in Argon2:
serial replies were noticed late and `sleep_until` deadlines (pulse
ends) fired late (issue #1388).

The middleware therefore separates *proving* a credential from
*recognising* one it has already proved:

- **Memo.** Each `rp_auth::layer` call builds one verifier holding a
  64-byte key drawn from the OS RNG. A presented credential is reduced
  to a keyed BLAKE2b-256 tag over a domain string and the
  length-prefixed username, password and stored PHC string. The memo
  holds one positive slot — one `AuthConfig` admits exactly one
  credential, so there is nothing to evict — and a ring of at most
  eight negative tags. A positive hit answers in microseconds and
  refreshes a 15-minute sliding idle TTL; on expiry the slot is cleared
  and zeroized. A negative hit answers 401 for 5 s, so a stale poller
  with an old password costs one KDF per 5 s instead of one per poll.
  Tags are PRF outputs under a key that never leaves the process: the
  memo holds no password, no hash and nothing a config-file attacker
  can use, and a hit still requires presenting the full credential, so
  it is not a bearer artefact. The key is per layer instance — never
  shared across services, logged or persisted — and a config reload
  rebuilds the router and therefore the memo. If the OS RNG cannot
  supply a key, the memo and the single-flight are disabled and every
  request takes the gated KDF path, so concurrent requests for one
  credential serialise on the permit and may be answered 503; the gate
  itself does not depend on the key, and a fixed key is never
  substituted.
- **Gate.** A miss runs the KDF on tokio's blocking pool behind a
  one-permit semaphore. The permit and the memo store live *inside* the
  blocking closure, so a client that disconnects mid-verify (an HTTP/2
  `RST_STREAM`, a TCP reset) can neither release the gate early nor lose
  the warm-up. At most one Argon2id computation — one core, 19 MiB — is
  in flight per service whatever clients do; the inline design allowed
  one per worker, and an ungated `spawn_blocking` would allow 512.
- **Single-flight by tag.** A request presenting the credential that is
  currently being verified waits for that verdict and is never refused,
  whether it found the verification in flight on arrival or only after
  queueing for the permit (two cold requests for one credential that
  both queue behind a third credential's KDF run one KDF between them:
  the one that loses the permit race becomes a waiter on the winner's
  verdict when its permit wait runs out). The client's own timeout
  remains the ceiling. Only a request for a *different* credential may
  be answered `503 Service Unavailable` with `Retry-After: 1`, after
  waiting 1 s for the permit and finding it still held by that other
  credential. **The auth layer must never answer 5xx for a credential
  that is being verified:** rp's SafetyMonitor read is fail-unsafe and
  parks the mount on any error, and sentinel's probe reads 503 as
  degraded (alive) but any other unexpected status as down. The 1 s
  bound sits under sentinel's 2 s probe timeout.
- **Verdict rules.** The username is compared in constant time (as
  fixed-length digests) and ANDed with the KDF result *after* the KDF
  has run, so a wrong username costs the same as a wrong password; the
  previous short-circuit answered a wrong username in 0.07 ms and a
  right one in 41 ms, a username oracle. A `password_hash` that Argon2
  could never accept a password against — not a PHC string, another
  algorithm's identifier, parameters Argon2 rejects, a salt under 8
  bytes, no hash field — is reported once at startup with the reason,
  and misses are then verified against a syntactically valid decoy
  whose hash field is random bytes with no known preimage, so the
  server fails closed with wrong-password timing instead of a
  microsecond 401. A KDF that panics writes nothing to the memo and
  answers 401.
- **What does not change.** `password_hash` stays an Argon2id PHC
  string with the crate's default parameters (m=19456, t=2, p=1);
  `doctor auth hash-password` and `doctor auth rotate` emit the same
  hashes; the wire format, the 401 challenge and the config schema are
  untouched; `rp_auth::layer` and `rp_auth::credentials::{hash_password,
  verify_password}` keep their signatures. No new crates: `blake2`,
  `subtle` and `zeroize` were already in the dependency graph.

Accepted residuals:

- A process-memory dump reduces a *human-chosen* password to fast-hash
  cracking for the lifetime of the layer instance. This is not a new
  class: the un-wiped Argon2 block buffer of the most recent verify is
  an equivalent BLAKE2b-speed oracle, and the Basic header sits in
  every live connection's read buffer. A doctor-minted credential
  (~190 bits of entropy) is unaffected under any of them.
- Under a sustained spray of distinct wrong passwords from a host on the
  LAN or VPN (≥ 25 requests/s), the single permit stays busy and a
  *cold* legitimate client — first request after a service start, a
  reload, or 15 minutes idle — is refused with 503 until the spray
  stops. Warm clients are unaffected. Per-peer limiting would close
  this and needs the accepted connection's peer address exposed to the
  router, which `rusty-photon-tls` does not do today; the refusals are
  counted and logged so the condition is visible.
- The gate bounds work per process, not per host: a spray against all
  services on one Pi still runs one KDF per service on four cores.
- The first request per service after a start or reload still pays one
  41 ms verify, now off the worker threads. On a rig running sentinel
  over TLS with `service_auth` configured, sentinel's 30 s probe is the
  de facto warm-up.

### Shared Crate: rp-auth

Authentication logic lives in a new workspace crate, `crates/rp-auth`,
following the same pattern as `crates/rp-tls`:

- `credentials.rs` — Argon2id hashing and verification
- `memo.rs` — the verification memo (keyed tags, positive slot,
  negative ring, TTLs)
- `verifier.rs` — the per-layer verifier: memo lookup, single-flight by
  tag, the one-permit gate and the off-worker KDF
- `middleware.rs` — axum/tower authentication layer
- `config.rs` — `AuthConfig` struct (username, password_hash)

This avoids duplicating auth logic across services.

### Service Config Changes

Each service gains an optional `auth` section nested under `server`:

```json
{
  "server": {
    "port": 11112,
    "tls": {
      "cert": "~/.rusty-photon/pki/certs/ppba-driver.pem",
      "key": "~/.rusty-photon/pki/certs/ppba-driver-key.pem"
    },
    "auth": {
      "username": "observatory",
      "password_hash": "$argon2id$v=19$m=19456,t=2,p=1$..."
    }
  }
}
```

When `auth` is absent or null, the service runs without authentication.
Authentication is opt-in and non-breaking.

### Server Startup (per service)

The router wrapping extends the existing TLS branch from ADR-002:

```rust
let router = server.into_router();

// Layer authentication if configured
let router = match &config.server.auth {
    Some(auth) => rp_auth::layer(router, auth),
    None       => router,
};

// Bind and serve (TLS or plain)
let listener = bind_dual_stack(addr)?;
match &config.server.tls {
    Some(tls) => serve_with_tls(listener, router, tls).await,
    None      => serve_plain(listener, router).await,
}
```

Authentication is applied before TLS wrapping — the middleware operates
at the HTTP layer regardless of whether the transport is encrypted.

### Client-Side Configuration

`rp` and `sentinel` (which are HTTP clients of other services) gain
optional auth configuration per target service:

```json
{
  "services": {
    "filemonitor": {
      "url": "https://localhost:11111",
      "auth": {
        "username": "observatory",
        "password": "my-secret-password"
      }
    }
  }
}
```

Client-side passwords are stored in plaintext in the config file (the
client needs the actual password to send Basic Auth headers). This is the
same model used by the ASCOM .NET library's `AlpacaConfiguration`.
File permissions (`chmod 600`) are the recommended protection.

### Discovery Server

The Alpaca discovery server (UDP multicast, port 32227) is unaffected.
Discovery only advertises the service port; it carries no credentials or
sensitive data. Clients discover the port, then authenticate when making
HTTP requests.

### Auth + TLS Interaction

Authentication without TLS sends credentials in cleartext over the
network. While the implementation does not prevent this combination (a
user might have other transport security such as a VPN), a startup
warning is logged:

```
WARN: Authentication is enabled but TLS is not. Credentials will be
      transmitted in cleartext. Consider enabling TLS (see `doctor --fix`).
```

### Password Recovery

If a user forgets their password, they edit the service config file
directly — remove the `auth` section or replace the `password_hash`
with a new value from `doctor auth hash-password`. No separate recovery mechanism
is needed.

## Consequences

### What Changes

- New workspace crate: `crates/rp-auth` (Argon2id hashing, tower
  middleware, config)
- `rp` gains a `hash-password` subcommand (since moved to `doctor auth
  hash-password`, ADR-016)
- Each service's `ServerBuilder` gains auth middleware wrapping (~10
  lines)
- Service configs gain an optional `auth` section
- Client configs (`rp`, `sentinel`) gain optional per-service auth
  credentials
- New workspace dependency: `argon2` crate (pure Rust)

### What Doesn't Change

- Plain HTTP without auth remains the default — no existing setup breaks
- TLS infrastructure (ADR-002) is unchanged
- The Alpaca protocol itself is unchanged
- The discovery protocol is unchanged
- Third-party Alpaca devices are accessed using whatever auth their
  client supports
- No scopes, roles, or fine-grained access control

### User Experience

```bash
# One-time setup
doctor auth hash-password
# Enter password: ********
# $argon2id$v=19$m=19456,t=2,p=1$...

# Paste hash into service config under [server.auth]
# Enter username/password in NINA, SGPro, or other Alpaca client
```

### Security Properties

| Property              | Without TLS        | With TLS           |
|-----------------------|--------------------|--------------------|
| Credential secrecy    | None (cleartext)   | Yes (encrypted)    |
| Replay protection     | None               | Yes (TLS session)  |
| Tampering protection  | None               | Yes (TLS integrity) |
| Credential storage    | Argon2id hash      | Argon2id hash      |
| Brute-force resistance| Argon2id cost      | Argon2id cost      |
| Per-request verify cost | µs on a warm memo; one gated 41 ms KDF per credential per 15 min idle | same |
| Verification DoS      | One KDF in flight per service, off the worker threads | same |
| Username enumeration  | Constant-time compare, KDF always runs | same |
| Credential in process memory | Keyed tag only; a dump is a fast-hash target for a human-chosen password (accepted) | same |

## Future: API Keys as a Secondary Mechanism

A future enhancement could add API key support alongside Basic Auth.
This would address scenarios where machine-to-machine tokens are more
convenient than username/password pairs:

- **Automation scripts** that should not embed plaintext passwords
- **Per-client revocation** — revoke one key without changing the
  password for all clients
- **Third-party integrations** where sharing a personal password is
  undesirable

The implementation would accept either `Authorization: Basic` or
`Authorization: Bearer <key>` (or `X-Api-Key: <key>`), with keys
generated via `rp generate-api-key` and stored as Argon2id hashes in the
service config. This matches the dual-auth model used by OctoPrint
(username/password + API keys) and Home Assistant (OAuth + long-lived
tokens).

This is explicitly deferred — Basic Auth alone satisfies all current
requirements and is the only mechanism compatible with existing ASCOM
Alpaca clients.

## Future: Rate Limiting

Authentication opens the door for brute-force attacks against the
password. Argon2id's cost is **not** the online defence — NIST SP 800-63B
treats verifier cost as protection for the stored hash and a throttle as
the online control — and since 2026-10-03 the verify runs behind a
one-permit gate that bounds the *work* an attacker can cause per service
(one core, 19 MiB), not the number of guesses they can make. A future
per-peer limiter (e.g. exponential backoff after 5 failures from one
address) needs the accepted connection's peer address exposed to the
router, which `rusty-photon-tls` does not do today, and it must count
negative-memo hits as failed presentations or repeated guesses inside the
5 s negative window become invisible to it. Any refusal it answers must be
`503`, never `429`: sentinel reads 503 as degraded and any other unexpected
status as down.

## Rejected: cheaper per-request verification (2026-10-03)

Considered for issue #1388 and rejected, so they are not reopened:

- **Argon2id parameter tuning** — every OWASP-compliant set measured
  36–58 ms on the Pi 5; the sets are equal-cost by design.
- **Below-OWASP parameters** — m=4096,t=3 is still 11.6 ms inline and
  weakens every hand-typed password.
- **A cheaper stored form for the doctor-minted credential only** — the
  fleet must keep working with hand-set passwords, so the verifier
  cannot assume a high-entropy secret; the memo serves both.
- **Switching KDF (bcrypt, scrypt, PBKDF2)** — any compliant setting is
  at least Argon2id-cost on a Cortex-A76; ADR-003 already preferred
  Argon2id over OmniSim's PBKDF2.
- **Memory pre-allocation / buffer reuse** — saves 2 ms of 41.
- **Offload only (`spawn_blocking` without a memo)** — unfreezes the
  runtime but leaves 41 ms of latency and ~10 % of the Pi's CPU on
  Argon2 during a NINA session.
- **Memo keyed by the raw `Authorization` header** — same latency in
  fewer lines, but keeps the base64 credential in memory for the TTL;
  the keyed tag costs 0.5 µs more and holds nothing crackable.
- **Session cookie or bearer token** — Alpaca clients only send Basic
  (Option 4 above).
- **Per-connection authentication state** — the router sees no
  connection identity in 22 of 23 services, and ui-htmx closes every
  proxied connection by design; the memo subsumes it.
- **A reverse proxy terminating auth on the Pi** — a second process and
  hop on four cores for 16 ports, and a re-plumb of ADR-002.
- **Exempting `/health` or the management API** — does nothing for
  device polls and would remove sentinel's probe as the warm-up.

## References

- [RFC 7617 — The 'Basic' HTTP Authentication Scheme](https://www.rfc-editor.org/rfc/rfc7617.html)
- [RFC 7235 — HTTP/1.1 Authentication](https://www.rfc-editor.org/rfc/rfc7235.html)
- [RFC 9106 — Argon2 Memory-Hard Function](https://www.rfc-editor.org/rfc/rfc9106.html)
- [OWASP Password Storage Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html)
- [ASCOM Alpaca API — security: \[\]](https://ascom-standards.org/api/)
- [ASCOM.Alpaca.Simulators — AuthorizationFilter](https://github.com/ASCOMInitiative/ASCOM.Alpaca.Simulators)
- [ASCOM Library — AlpacaConfiguration](https://github.com/ASCOMInitiative/ASCOMLibrary)
- [argon2 crate](https://crates.io/crates/argon2) — pure Rust Argon2id
- [axum-auth crate](https://crates.io/crates/axum-auth) — Basic/Bearer
  extractors for axum
- [ADR-002 — TLS for Inter-Service Communication](./002-tls-for-inter-service-communication.md)

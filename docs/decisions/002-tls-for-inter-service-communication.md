# ADR-002: TLS for Inter-Service Communication

## Status

Accepted

## Updates

**2026-07-21** — Self-signed service certs now carry an Authority Key
Identifier extension pointing at the issuing CA's Subject Key
Identifier, and their own SKI
([#621](https://github.com/rusty-photon/rusty-photon/issues/621)):
`cert::generate_service_cert` (`services/doctor/src/provision/cert.rs`)
sets `use_authority_key_identifier_extension` and switches
`is_ca` from `IsCa::NoCa` to `IsCa::ExplicitNoCa`, which is what makes
`rcgen` write the leaf's own SKI. RFC 5280 §4.2.1.1 makes AKI a MUST for
CA-issued (non-self-signed) certs, and §4.2.1.2 makes SKI a SHOULD for
leaves; without either, strict verifiers (Python 3.13's default
`VERIFY_X509_STRICT`) reject the cert even though OpenSSL's non-strict
path (curl, current reqwest backends) accepts it. The CA's own cert
already carried an SKI (rcgen writes it for any `IsCa::Ca`), so no
change was needed there. `crates/rusty-photon-tls/src/test_cert.rs`
(test-fixture certs) got the same two-field change, so tests exercise
the same shape as production. Existing installs converge at the next
renewal — but renewal only re-issues inside its 30-day window ahead of
a 10-year expiry, so a pair issued the day before this fix stays
non-compliant for close to a decade on an otherwise-untouched host. An
operator who needs the new extensions sooner runs `doctor tls issue
--force`, which re-issues every service pair from the existing CA
immediately (see §Provisioning below; the CA itself is untouched and
never needs replacing for this).

**2026-07-20** — `doctor tls renew` now parses `<config-root>/renew.env`
(`KEY=VALUE` per line) and consults it as a fallback when resolving
`dns_credentials`
([#615](https://github.com/rusty-photon/rusty-photon/issues/615)): the
shipped systemd unit set only `RUST_LOG`/`HOME`, so a `$VAR`-indirected
credential (this ADR's env-var form) had nothing to resolve from on a
timer-driven 3am run — the only working shape on a packaged host was the
literal token in `acme.json`. Rather than three platform-specific fixes
(systemd `EnvironmentFile=`, a launchd `EnvironmentVariables` plist key,
a Windows machine-level env var), the parsing lives in the renewal leg
itself, so the same file convention works identically on all three
platforms. Missing file is not an error; a key already set in the
process environment always wins over the file — `renew.env` is consulted
as a map, not injected with `std::env::set_var`, since the renewal leg
runs on a multi-thread Tokio runtime where mutating the process
environment races any concurrent read. See docs/services/doctor.md
§Renewal.

**2026-07-19** — `serve_tls_with_acceptor`
(`crates/rusty-photon-tls/src/server.rs`) now sniffs the first byte of
every accepted connection before handing it to the TLS acceptor
([#610](https://github.com/rusty-photon/rusty-photon/issues/610)): a TLS
handshake record (`0x16`) proceeds exactly as before; anything else is
treated as a plaintext HTTP request and answered with a `308 Permanent
Redirect` to `https://` on the same host and port, bounded by a
5-second timeout and an 8 KiB header cap so a misdirected `http://`
bookmark — or a probe — cannot become a resource sink on the TLS port.
Bytes that don't parse as an HTTP request head within the bound are
dropped with no response. See §Plaintext HTTP Redirect below.

**2026-07-19** — Cloudflare zone resolution walks parent labels and
**requires the domain to sit below the zone apex**
([#613](https://github.com/rusty-photon/rusty-photon/issues/613)):
`--domain rig.example.com` resolves to the `example.com` zone, and
`--domain example.com` is rejected — the `<service>.<host>.<domain>`
pattern this ADR's examples always showed. Previously the lookup was an
exact zone-name match, which both failed for sub-label domains and
forced the wildcard to `*.<zone>`, where it is also valid for every
sibling hostname in the zone — a key on an observatory machine must not
cover unrelated infrastructure names. Consequence for §Multi-Machine
Support: each machine can issue its own certificate under its own host
label instead of receiving a copy of one shared key; `post_renewal_hooks`
distribution remains for deployments that want the single shared scope
(service names under one host label resolving to different machines).

**2026-07-18** — The certificate lifecycle was re-homed from `rp` to
**doctor** (ADR-016 decision 10; the doctor plan's D6): `rp init-tls` is
now `doctor tls issue [--acme]`, hashing is `doctor auth hash-password`,
and all material lives **flat** under `<config-root>/pki` (no `certs/`
subdirectory) with `acme.json` beside the service configs —
`~/.rusty-photon/` is retired. Read path snippets in the historical
sections below through that translation. In the same pass renewal
([#541](https://github.com/rusty-photon/rusty-photon/issues/541)) was
**implemented, with a different scheduler than first designed here**: a
one-shot `doctor tls renew` on a platform timer instead of a background
task in `rp serve` — renewing any one service's certificate must not
require another service to be running. §Automatic Certificate Renewal and
§Certificate Hot-Reloading below were rewritten to the as-built design,
and §Implementation Phases now carries status markers (their earlier
absence let this ADR describe unbuilt renewal in the present tense for
months — see #541). The operator-facing procedure lives in
`docs/services/doctor.md` §Provisioning/§Renewal; this ADR keeps the
rationale.

**2026-04-22** — Upstream `ascom-alpaca-rs` replaced the exposed
`Server::into_router() -> axum::Router` with `Server::into_service() ->
AlpacaService` (an opaque `tower::Service`). Services now wrap it back
into an `axum::Router` via `Router::new().fallback_service(...)` to keep
the rest of the integration (`rp-auth` layering, `rp-tls` binding)
unchanged. The decision below still stands — router-level extension is
owned by Rusty Photon; only the upstream adapter shape changed. See
current service code in `services/*/src/lib.rs` for the live pattern.

## Context

All Rusty Photon services communicate over plain HTTP. ASCOM Alpaca is an
HTTP-based protocol with no built-in security. On a typical observatory
network (Raspberry Pi, local Wi-Fi), traffic between services —
including equipment commands, sensor readings, and session state — is
unencrypted and unauthenticated.

While the threat model for an isolated observatory network is modest,
unsecured HTTP means any device on the network can observe or inject
commands. As remote observatory setups become more common (VPN access
over the internet, shared club networks), the risk grows.

The goal is to enable HTTPS between services we control, without
requiring users to manage certificates manually, and without forcing TLS
on third-party Alpaca devices that don't support it.

## Options Considered

### Option 1: TLS Support Inside ascom-alpaca-rs Crate

Add a `tls` feature flag to the upstream `ascom-alpaca` crate with
`rustls` and `tokio-rustls` dependencies. The crate's `Server::bind()`
would accept an optional `TlsConfig` and wrap the listener internally.

**Pros:**
- All Alpaca servers get TLS automatically
- Single implementation shared across all services

**Cons:**
- Adds TLS dependencies to a general-purpose ASCOM crate that many
  consumers may not need
- Couples TLS policy (cert paths, cipher suites) to the upstream crate
- Harder to maintain — TLS changes require upstream releases
- The crate currently has zero TLS awareness; this is a significant
  scope expansion for a library focused on the Alpaca protocol

### Option 2: Expose Router, Handle TLS in Rusty Photon (Chosen)

Make `ascom-alpaca`'s `Server::into_router()` method public (currently
private). Rusty Photon services extract the `axum::Router`, handle
socket binding and optional TLS wrapping themselves.

**Pros:**
- Minimal upstream change — one visibility modifier (`fn` to `pub fn`)
- All TLS logic lives in Rusty Photon where it belongs
- Rusty Photon controls cert paths, cipher config, and the
  TLS-vs-plain-HTTP decision
- The `ascom-alpaca` crate stays focused on protocol correctness
- Services that don't need TLS are unaffected

**Cons:**
- Rusty Photon must replicate the socket2 dual-stack binding logic from
  `ascom-alpaca` (roughly 15 lines)
- Discovery server must be started separately (already public API)
- Each rusty-photon service's `ServerBuilder` needs a small TLS branch

### Option 3: axum-server with rustls in Rusty Photon (No Upstream Change)

Use the `axum-server` crate to replace the entire server lifecycle,
bypassing `ascom-alpaca`'s bind/start entirely. Build the Alpaca routes
manually or via a hypothetical device-registration-only API.

**Pros:**
- Zero upstream changes

**Cons:**
- Requires duplicating or reverse-engineering all Alpaca route
  registration, management endpoints, and the setup page
- Fragile — breaks when ascom-alpaca adds new routes or changes
  internal structure
- Defeats the purpose of using the crate

### Option 4: TLS Termination Proxy (e.g., Caddy, nginx)

Run a reverse proxy in front of each service that terminates TLS and
forwards plain HTTP to the service.

**Pros:**
- Zero code changes to any service
- Battle-tested TLS implementations

**Cons:**
- Additional process per service (6+ proxies on a Raspberry Pi)
- Significant memory and CPU overhead on constrained hardware
- Complex deployment — users must configure and maintain proxy configs
- Adds latency to every request
- Contradicts the "minimal footprint" tenet

## Decision

We chose **Option 2: Expose Router, Handle TLS in Rusty Photon**.

The upstream change is minimal — making one method public. All TLS
complexity stays in our codebase where we can iterate on it freely. This
keeps `ascom-alpaca-rs` focused on protocol correctness and avoids
burdening the upstream crate with security policy decisions.

## Implementation

### Upstream Change (ascom-alpaca-rs)

Make `Server::into_router()` public. This is the only required change.
The discovery server is already public (`DiscoveryServer`,
`BoundDiscoveryServer`).

### Certificate Management

A new `rp init-tls` subcommand generates all certificates on first run:

1. **Root CA** — a self-signed CA certificate ("Rusty Photon Observatory
   CA"), valid for 10 years. Stored in `~/.rusty-photon/pki/ca.pem` and
   `ca-key.pem`.
2. **Per-service certificates** — one cert per configured service,
   signed by the CA. SANs include `localhost`, `127.0.0.1`, the system
   hostname, and any IPs from service configs. Stored in
   `~/.rusty-photon/pki/certs/{service-name}.pem` and
   `{service-name}-key.pem`.

Certificate generation uses the `rcgen` crate (pure Rust, no OpenSSL
dependency), keeping the build simple on all platforms including
Raspberry Pi.

Long-lived certificates (10 years) are appropriate because:
- This is a private network with a single operator
- There is no revocation infrastructure to maintain
- Observatory setups are not frequently reprovisioned

### Service Config Changes

Each service gains an optional `tls` section:

```json
{
  "server": {
    "port": 11112,
    "tls": {
      "cert": "~/.rusty-photon/pki/certs/ppba-driver.pem",
      "key": "~/.rusty-photon/pki/certs/ppba-driver-key.pem"
    }
  }
}
```

When `tls` is absent or null, the service runs plain HTTP as before.
TLS is opt-in and non-breaking.

### Server Startup (per service)

Each service's `ServerBuilder` gains a TLS branch:

```rust
let router = server.into_router();
let listener = bind_dual_stack(addr)?; // replicates socket2 logic

match &config.server.tls {
    Some(tls) => serve_with_tls(listener, router, tls).await,
    None      => axum::serve(listener, router.into_make_service()).await,
}
```

The dual-stack binding logic (~15 lines of socket2 code) is extracted
into a shared utility in the workspace.

### Client-Side Trust (rp, sentinel)

`rp` and `sentinel` are Alpaca HTTP clients. When a CA cert is
configured, they add it to the `reqwest` client:

```rust
let client = reqwest::Client::builder()
    .add_root_certificate(ca_cert)
    .build()?;
```

URLs in config use `https://` for TLS-enabled services and `http://`
for third-party Alpaca devices. Both work — the client follows the
scheme in the URL.

### Discovery Server

The Alpaca discovery server (UDP multicast, port 32227) is unaffected.
It advertises the service port; clients then connect over HTTPS if
configured. Discovery itself does not carry sensitive data.

### Plaintext HTTP Redirect (as built — issue #610)

Every TLS-enabled service listens on a single port, so a browser
following an old `http://` bookmark, or a client with a stale scheme in
its config, previously got a raw TLS handshake-failure alert dumped as
text — undiagnosable mojibake rather than a clear error.

`serve_tls_with_acceptor` peeks the first byte of each accepted
connection without consuming it: `0x16` (a TLS handshake record, RFC
8446 §5.1) hands the connection to the `TlsAcceptor` exactly as before.
Any other first byte is treated as a possibly-plaintext HTTP request:

1. The request head (request line + headers, up to the blank line) is
   read with an 8 KiB size cap and a 5-second total timeout — both
   apply to the initial byte peek too, so a connection that sends
   nothing, or trickles bytes forever, cannot hold a task open
   indefinitely.
2. If the buffered bytes parse as `METHOD target HTTP/x.y`, the server
   replies with a minimal `308 Permanent Redirect` to
   `https://<host>:<port><target>` — `<host>` is the client's `Host`
   header (port stripped) or, failing that, the connection's own local
   IP; `<port>` is always the TLS listener's own port, never a port the
   client happened to claim, so the redirect always lands back on the
   same TLS port — then the connection closes.
3. Anything that doesn't resolve to a parseable request within the
   bound (garbage bytes, a connection that never sends a full request
   line) is dropped without a response — only bytes that look like
   HTTP earn a reply on the TLS port.

Every TLS-enabled service gets this behavior for free through the
shared accept loop; no per-service wiring is needed. curl and other API
clients hitting `http://` get a clean `308` instead of a
connection-level failure, which also makes a scheme misconfiguration
diagnosable — complementing the doctor join checks
(`docs/services/doctor.md`).

## Consequences

### What Changes

- `ascom-alpaca-rs`: one method becomes public (`into_router`)
- Each service's `ServerBuilder` gains ~20 lines of TLS branching
- A shared `bind_dual_stack()` utility is added to the workspace
- `rp` gains an `init-tls` subcommand
- Service configs gain an optional `tls` section
- New workspace dependencies: `rcgen`, `rustls`, `tokio-rustls`,
  `rustls-pemfile`

### What Doesn't Change

- Plain HTTP remains the default — no existing setup breaks
- Third-party Alpaca devices (cameras, mounts) are accessed over
  whatever scheme their URL specifies
- The Alpaca protocol itself is unchanged
- The discovery protocol is unchanged
- Service-to-service communication patterns are unchanged

### User Experience

```bash
# One-time setup
rp init-tls

# That's it. Services read certs from config.
# To go back to plain HTTP, remove the tls section from configs.
```

### Platform Support

- `rcgen` and `rustls` are pure Rust — no system OpenSSL dependency
- Works on Linux (x86_64, ARM64), macOS, and Windows
- No impact on Raspberry Pi builds

## ACME / Let's Encrypt Support

### Motivation

The self-signed CA approach requires every client to be configured with
the CA certificate. Third-party Alpaca devices with publicly-signed
certificates cannot be mixed with self-signed services in a single
sentinel configuration without a custom certificate verifier. Publicly-
trusted certificates from Let's Encrypt remove both limitations: browsers
and clients trust Let's Encrypt natively, and no manual CA installation
is needed.

### Why DNS-01 Is the Only Viable Challenge Type

Observatory services run on local networks behind NAT. They are not
reachable on ports 80 or 443 from the internet. Of the three ACME
challenge types, only DNS-01 works in this environment:

| Challenge    | Requires                  | Behind NAT? | Wildcards? |
|------------- |---------------------------|-------------|------------|
| HTTP-01      | Port 80 from internet     | No          | No         |
| TLS-ALPN-01  | Port 443 from internet    | No          | No         |
| **DNS-01**   | **DNS TXT record via API** | **Yes**     | **Yes**    |

DNS-01 proves domain ownership by creating a TXT record at
`_acme-challenge.<DOMAIN>`. Let's Encrypt validates via public DNS. The
actual services can live on `192.168.x.x` behind a home router — they
never need to be publicly accessible.

### User Experience

#### One-Time Setup

```bash
rp init-tls --acme \
  --domain observatory.example.com \
  --dns-provider cloudflare \
  --dns-token "$CLOUDFLARE_API_TOKEN" \
  --email user@example.com
```

This single command:

1. Creates an ACME account with Let's Encrypt
2. Requests a wildcard certificate for `*.observatory.example.com`
3. Creates a DNS TXT record via the Cloudflare API for validation
4. Waits for DNS propagation, completes the challenge
5. Writes the certificate chain and private key to
   `~/.rusty-photon/pki/certs/`
6. Prints configuration hints for each service

After this, services use publicly-trusted certificates. No CA
installation on clients. No manual renewal.

#### Dual Path (Self-Signed Remains Default)

Users without a domain (or on air-gapped networks) keep using the
self-signed CA. ACME is an opt-in alternative:

```bash
rp init-tls                                          # self-signed CA
rp init-tls --acme --domain observatory.example.com  # Let's Encrypt
```

#### Staging Environment

Let's Encrypt provides a staging endpoint with much higher rate limits
and untrusted root CAs. Users should test with staging before switching
to production:

```bash
rp init-tls --acme --staging --domain observatory.example.com ...
```

The `--staging` flag uses `acme-staging-v02.api.letsencrypt.org`. This
is the recommended first step to verify DNS provider credentials and
challenge flow without risking rate limit exhaustion.

### Single Wildcard Certificate

All services share one wildcard certificate
(`*.observatory.example.com`) rather than per-service certificates.

**Rationale:**

- One ACME order, one DNS challenge, one renewal — not five
- Let's Encrypt rate limits (50 certs/domain/week) are a non-issue
  with a single cert (renewals are exempt from rate limits)
- Each service is distinguishable by subdomain (e.g.,
  `filemonitor.observatory.example.com`,
  `rp.observatory.example.com`)

**Trade-off:** A compromised key exposes all services. This is
acceptable for single-machine deployment where all services share the
same trust boundary. For multi-machine setups, the same cert is
distributed to each machine — the trust boundary is the observatory
network, not individual hosts.

### Multi-Machine Support

The wildcard certificate is valid for all subdomains regardless of which
IP they resolve to. Services on different machines simply need a copy of
the same cert and key files:

```
*.observatory.example.com cert covers all of:

filemonitor.observatory.example.com  →  192.168.1.50  (Pi in the dome)
rp.observatory.example.com           →  192.168.1.50  (same Pi)
camera.observatory.example.com       →  192.168.1.51  (different machine)
mount.observatory.example.com        →  192.168.1.52  (yet another)
```

Users configure split-horizon DNS so domain names resolve to local IPs.
Common approaches: Pi-hole/dnsmasq overrides, local hosts file entries,
or Tailscale MagicDNS. The public DNS records exist only for ACME
challenge validation — they do not need to resolve to anything
meaningful externally.

Certificate distribution to remote machines is handled via configurable
`post_renewal_hooks` in the ACME config (see Configuration section).

### Configuration

ACME config is **standalone** at `~/.rusty-photon/acme.json`, not
embedded in any service config. This decouples certificate management
from any specific service and supports multi-machine deployments where
the ACME client runs on one host:

```json
{
  "email": "user@example.com",
  "domain": "observatory.example.com",
  "dns_provider": "cloudflare",
  "dns_credentials": {
    "api_token": "$CLOUDFLARE_API_TOKEN"
  },
  "staging": false,
  "renewal_days_before_expiry": 30,
  "post_renewal_hooks": [
    "scp ~/.rusty-photon/pki/certs/acme-*.pem pi-dome:~/.rusty-photon/pki/certs/"
  ]
}
```

| Field                         | Required | Description |
|-------------------------------|----------|-------------|
| `email`                       | Yes      | ACME account email for expiry notifications |
| `domain`                      | Yes      | Base domain (wildcard cert issued for `*.<domain>`) |
| `dns_provider`                | Yes      | DNS provider identifier (e.g., `"cloudflare"`) |
| `dns_credentials`             | Yes      | Provider-specific credentials; values starting with `$` are read from environment variables |
| `staging`                     | No       | Use Let's Encrypt staging endpoint (default: `false`) |
| `renewal_days_before_expiry`  | No       | Days before expiry to trigger renewal (default: `30`) |
| `post_renewal_hooks`          | No       | Shell commands to run after successful renewal (e.g., distribute certs to remote machines) |
| `directory_url` (D6b)         | No       | Full ACME directory URL, overriding the Let's Encrypt endpoints — an internal ACME CA (step-ca), or Pebble in tests |
| `acme_root` (D6b)             | No       | Path to a PEM trust anchor for the ACME server's own TLS endpoint (private directories are not publicly trusted) |
| `dns_propagation_seconds` (D6b) | No     | Wait between writing the TXT record and requesting validation (default: `15`) |

#### File Layout (as of D6a: flat, under the config root)

```
<config-root>/                 # e.g. /var/lib/rusty-photon/.config/rusty-photon
├── acme.json                  # ACME configuration (standalone, 0600)
├── pki/
│   ├── acme-account.json      # ACME account credentials (persistent, 0600)
│   ├── acme-cert.pem          # Wildcard certificate chain
│   ├── acme-key.pem           # Wildcard private key (0600)
│   ├── ca.pem                 # Self-signed CA (unused with ACME)
│   ├── ca-key.pem             # (0600)
│   └── credential             # Observatory credential (ADR-016; 0600)
```

#### Service Config (Unchanged Structure)

Each service still uses the existing `tls` section. With ACME, all
services point to the same wildcard cert files:

```json
{
  "server": {
    "port": 11112,
    "tls": {
      "cert": "~/.rusty-photon/pki/certs/acme-cert.pem",
      "key": "~/.rusty-photon/pki/certs/acme-key.pem"
    }
  }
}
```

On remote machines the paths are the same — certs arrive via
`post_renewal_hooks`.

### Pluggable DNS Providers

DNS provider interaction is behind a trait so new providers can be added
without touching ACME logic:

```rust
#[async_trait]
pub trait DnsProvider: Send + Sync + Debug {
    /// Create a TXT record for the ACME challenge.
    async fn create_txt_record(&self, fqdn: &str, value: &str) -> Result<()>;

    /// Remove the TXT record after validation completes.
    async fn delete_txt_record(&self, fqdn: &str) -> Result<()>;
}
```

The initial implementation ships **Cloudflare** only (free tier, widely
used for domain management). The Cloudflare implementation uses the
official [`cloudflare`](https://crates.io/crates/cloudflare) Rust crate,
which handles authentication, error responses, zone ID lookup, and
retry logic out of the box.

Adding a new provider means implementing the `DnsProvider` trait and
registering the provider identifier in the config parser. No changes to
ACME orchestration, certificate management, or CLI.

### Automatic Certificate Renewal

Let's Encrypt certificates currently have a 90-day lifetime, moving to
45 days by February 2028. Automated renewal is non-negotiable.

#### Renewal Flow (as built — D6b)

Renewal is a **one-shot `doctor tls renew` run by a platform scheduler**
(systemd timer / Windows scheduled task / launchd interval; the units
ship in sentinel's package since D7). The originally-designed background task in
`rp serve` was rejected when renewal moved to doctor: it would make every
other service's certificate hostage to rp running, and doctor already
owns the rest of the lifecycle. The command is a no-op unless material
is inside its renewal window, so the same daily timer is correct for
self-signed installs (10-year certificates) and ACME installs alike.

When `<config-root>/acme.json` exists and `acme-cert.pem` is missing or
within `renewal_days_before_expiry` of its `not_after`:

1. Load the persisted settings from `acme.json` — directory URL, DNS
   provider and credentials (`$VAR` values resolve from the environment
   here, unattended), propagation wait, optional ACME trust root
2. Load or create the ACME account from `pki/acme-account.json`
3. Create a new order via `instant-acme`, complete the DNS-01 challenge
   via the configured `DnsProvider`, finalize, retrieve the chain —
   retrying a failed order up to 3 times (a failed authorization is
   dead; each attempt is a fresh order)
4. Write the new pair to `pki/acme-cert.pem` / `acme-key.pem` via
   write-then-rename, so a reloading service never reads a torn file
5. Running services pick the pair up in-process (see Hot-Reloading)
6. Execute `post_renewal_hooks` to distribute to remote machines; every
   hook runs, and any failure exits 2 — a silently-failed hook is a
   remote machine that expires unattended

Self-signed service pairs get the same treatment from the same command
(re-issued from the existing CA inside a 30-day window, SANs preserved);
the CA itself is never auto-renewed. On both legs a pair whose key half
cannot be loaded — or no longer matches the certificate — is due
regardless of the window: it cannot serve TLS now. The `tls.expiry` doctor check
reports expired or expiring material on every diagnosis — an expired
certificate otherwise loads cleanly and only *clients* reject the
handshake, invisibly to the server.

### Certificate Hot-Reloading (as built — D6b)

`with_single_cert()` bakes the certificate into the `ServerConfig` at
startup. For ACME, certificates must be swappable without restarting
services — a restart-based swap would have to be scheduled around
exposures, while an in-process swap removes the mid-exposure hazard
entirely.

`rusty-photon-tls`'s `build_tls_acceptor` builds the `ServerConfig` with
`.with_cert_resolver(...)` and a `ReloadableCertResolver`: a
`ResolvesServerCert` implementation backed by an
`RwLock<Arc<CertifiedKey>>` that also remembers the cert/key **paths and
mtimes**. On a TLS handshake — throttled to at most one check every
60 seconds — it re-stats both files and reloads the pair when an mtime
changed. There is no file-watcher, no signal handler, and no new
dependency; the mechanism is identical on Linux, macOS, and Windows, and
it covers certs that arrive by any route — `doctor tls renew` locally or
`post_renewal_hooks` `scp`-ing to a remote machine — provided the copy
updates the file's mtime (plain `scp`/`cp` do; timestamp-preserving
`scp -p` / `rsync -a` defeat the trigger).

A pair that fails to load, or whose key does not match its certificate
(rustls' `keys_match` — the guard for the moment between the two file
writes), is skipped with a debug log and the previous certificate keeps
serving until the next check; the swap is atomic behind the `RwLock`.
The change is invisible to services: self-signed certificates go through
the same resolver and simply never change.

### ACME Library: instant-acme

[`instant-acme`](https://crates.io/crates/instant-acme) (v0.8) is the
ACME protocol library. It is the only Rust crate supporting DNS-01
challenges and aligns with the project's existing dependency stack:

| Dependency   | instant-acme | Rusty Photon | Compatible? |
|------------- |------------- |------------- |-------------|
| `rcgen`      | Yes          | Yes (v0.13)  | Yes         |
| `tokio`      | Yes          | Yes          | Yes         |
| `rustls`     | Yes          | Yes (v0.23)  | Yes         |
| Crypto       | `aws-lc-rs`  | `aws-lc-rs`  | Yes         |

Features used: full RFC 8555 implementation (async), DNS-01 challenge
support, CSR generation via `rcgen`, serializable account credentials
for persistence, and certificate revocation.

### Client-Side Trust: No CA Configuration Needed

With the self-signed CA, clients (rp, sentinel) must be configured with
`ca_cert` and use `tls_certs_only()` in the reqwest builder to disable
platform root certificates. This workaround exists because the macOS
Security framework rejects self-signed CAs that are not in the system
keychain — the platform verifier and the custom CA conflict.

With Let's Encrypt, this problem disappears entirely. Let's Encrypt's
root CA is already in every platform's trust store (macOS, Windows,
Linux). Clients use the default reqwest builder with no `ca_cert`
configuration — the `build_reqwest_client(None)` path works on all
platforms without any special handling.

The `ca_cert` / `tls_certs_only()` code path remains for self-signed CA
users only.

### Security Considerations

| Concern                   | Mitigation |
|---------------------------|------------|
| DNS API token on disk     | `$ENV_VAR` syntax in config; file permissions restricted to 0600 |
| Private key storage       | Same `~/.rusty-photon/pki/` directory with 0600 permissions |
| ACME account key          | Separate from cert keys; stored in `acme-account.json` |
| Rate limit exhaustion     | Always test with `--staging` first; renewals are exempt from rate limits |
| Compromised cert key      | One wildcard cert = one key to protect; same trust boundary as self-signed |
| DNS credential blast radius | Optional CNAME delegation: `_acme-challenge.observatory.example.com CNAME _acme-challenge.acme.example.com` limits write access to a dedicated validation zone |

#### Let's Encrypt Rate Limits (Production)

| Limit                          | Value            |
|--------------------------------|------------------|
| Certificates per domain / week | 50 (renewals exempt) |
| Duplicate certificates / week  | 5                |
| Orders per account / 3 hours   | 300              |
| Auth failures per host / hour  | 5                |

For a single observatory with one wildcard cert, these limits are not a
practical concern.

### New Dependencies

```toml
# Workspace Cargo.toml [workspace.dependencies]
instant-acme = "0.8"
cloudflare = "0.12"
```

### Implementation Phases

#### Phase 1: Core ACME Infrastructure — **shipped** (re-homed to doctor in D6a)

- `DnsProvider` trait and Cloudflare implementation (now doctor modules)
- ACME account creation and persistence via `instant-acme`
- DNS-01 challenge solver (create record → wait for propagation →
  respond to challenge → clean up record)
- Certificate issuance flow (order → challenge → finalize → download)
- The `--acme` flags (originally on `rp init-tls`, now on
  `doctor tls issue`)

#### Phase 2: Certificate Hot-Reloading — **shipped** (D6b, reshaped)

- `ReloadableCertResolver` in `rusty-photon-tls` (mtime re-check on
  handshake — no watcher)
- `server.rs` uses `with_cert_resolver` (backward-compatible)
- One-shot `doctor tls renew` on a platform timer — **replacing** the
  originally-planned background task in `rp serve` and the separate
  `rp renew-tls` command (see Updates, 2026-07-18)

#### Phase 3: Polish and Testing — **shipped** (D6b)

- Staging/production toggle (`--staging`), plus `directory_url` /
  `acme_root` for non-Let's-Encrypt directories
- Configuration validation and helpful error messages
- BDD tests using [Pebble](https://github.com/letsencrypt/pebble)
  (Let's Encrypt's official ACME test server) for end-to-end flows
- `post_renewal_hooks` execution
- Documentation updates

### Future: DNS-PERSIST-01

A new challenge type approved by the CA/Browser Forum (October 2025).
Instead of creating a new TXT record for each renewal, the user sets a
persistent authorization record once:

```
_acme-challenge.observatory.example.com TXT "acme-persist=<value>"
```

This eliminates DNS provider API credentials from the renewal flow
entirely — credentials are needed only during initial setup.

Expected production availability: Q2 2026. When available, it would be
added as an alternative challenge strategy behind the same `DnsProvider`
trait, dramatically simplifying the ongoing user experience.

## References

- [rcgen crate](https://crates.io/crates/rcgen) — pure Rust X.509
  certificate generation
- [rustls](https://crates.io/crates/rustls) — modern TLS in Rust
- [instant-acme](https://crates.io/crates/instant-acme) — pure Rust
  ACME client for Let's Encrypt / ZeroSSL
- [Let's Encrypt challenge types](https://letsencrypt.org/docs/challenge-types/)
- [Let's Encrypt rate limits](https://letsencrypt.org/docs/rate-limits/)
- [Let's Encrypt staging environment](https://letsencrypt.org/docs/staging-environment/)
- [Let's Encrypt certificate lifetime changes](https://letsencrypt.org/2025/12/02/from-90-to-45)
- [DNS-PERSIST-01 announcement](https://letsencrypt.org/2026/02/18/dns-persist-01)
- [Pebble ACME test server](https://github.com/letsencrypt/pebble)
- [rustls ResolvesServerCert](https://docs.rs/rustls/latest/rustls/server/trait.ResolvesServerCert.html)
- [ASCOM Alpaca API](https://ascom-standards.org/api/)
- [axum TLS examples](https://github.com/tokio-rs/axum/tree/main/examples/tls-rustls)

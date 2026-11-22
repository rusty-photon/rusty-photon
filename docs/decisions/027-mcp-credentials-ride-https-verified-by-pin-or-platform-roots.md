# ADR-027: MCP credentials ride HTTPS verified by the pin or the platform roots

## Status

Accepted (2026-11-22, by the owner, on the first review of
[#1864](https://github.com/rusty-photon/rusty-photon/pull/1864)).
Supersedes in part
[ADR-017](017-standard-mcp-client-construction.md): in decision 3, the
clause that a configured credential without a configured CA is not
sent. The rest of decision 3 stands. A credential on a non-HTTPS URL is
still not sent, and plaintext credentials still never travel over
cleartext or unverified channels.

## Context

ADR-017 decision 3 took sentinel's credential policy verbatim: a
credential rides only HTTPS whose CA is configured. At the time, the only
TLS an observatory ran was doctor's self-signed CA, which a client can
verify only through a pin, so "a CA is configured" and "the connection
is verified" meant the same thing.

An ACME install separates them. Every service serves a publicly-trusted
wildcard certificate, and every client verifies it against the platform
roots. A `ca_cert` pin would *replace* those roots (`tls_certs_only`), so
doctor writes no pin on an ACME install and removes the self-signed one
it wrote before (doctor.md § What `--fix` adds, § ACME convergence).
Sentinel's probe client follows that model: with no pin, it authenticates
over a connection verified against the platform roots (sentinel.md
§ Configuration).

`rp-mcp-client` kept the older rule. On an ACME install with
`server.auth` on, every client built through it connected over verified
HTTPS, withheld the observatory credential because no CA was configured,
and got 401 on every call. That covered planetarium-bridge,
calibrator-flats, focus-model, polar-align, session-runner and ui-htmx
reaching rp, and rp reaching its tool providers. Doctor's join converges
those clients' URLs onto `https://rp.<domain>`, and doctor reported
nothing, since every field it judges was right.

## Decision

1. **`rp-mcp-client::basic_authorization` presents the credential over
   any `https://` URL.** It does so whether the connection is verified
   against a configured `ca_cert` or, with none configured, against the
   platform roots (`rusty_photon_tls::client::build_reqwest_client`).
   This is sentinel's probe policy, and it is the trust model doctor
   provisions on an ACME install.
2. **A credential on a non-HTTPS URL is still not sent.** The client
   connects unauthenticated and logs a loud warning, as before.
3. **A configured pin that cannot be loaded still withholds the
   credential.** `RpMcpClient::connect` fails when its CA cannot be read.
   session-runner's event stream, which falls back to default trust in
   that case, drops the credential with the pin. An operator who pinned a
   CA intended that CA, and the credential is never paired with a client
   that lost it.

## Consequences

- On an ACME install with auth on, every `rp-mcp-client` consumer
  authenticates to rp, and rp authenticates to its tool providers,
  without a `ca_cert`. Doctor's join on the MCP clients' URLs of rp
  therefore converges them to a working state on both kinds of install.
- On a self-signed install nothing changes. A client with no pin fails
  certificate verification against the self-signed rp before any
  credential is sent, and doctor's `joins.client-transport` reports that
  missing `ca_cert`.
- An operator who points a client at a public-CA HTTPS endpoint that is
  not rp would send it the credential. The credential is configured per
  client for rp alone, so that endpoint is the operator's configured
  target either way.

## References

- [ADR-017](017-standard-mcp-client-construction.md): standard MCP
  client construction, decision 3.
- [doctor.md § Client-target joins](../services/doctor.md#client-target-joins)
  and § What `--fix` adds: no pin on an ACME install.
- [sentinel.md](../services/sentinel.md): the probe client's credential
  over platform-root-verified TLS.

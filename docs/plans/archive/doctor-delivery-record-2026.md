# doctor: the delivery record, 2026-07 to 2026-09

A record, moved whole from `docs/services/doctor.md` on 2026-11-15: the
section that listed what each delivery of doctor brought — phases D2 to D7
of the [service-config-doctor plan](service-config-doctor.md) (July 2026),
the client-target joins as the issues that delivered them widened their
fix eligibility (July to September), and the ACME convergence checks and
the flip orchestrator of the [ACME flip plan](acme-flip.md) (September).
It stood under
[doctor.md § MVP scope](../../services/doctor.md#mvp-scope); that document
states what doctor does today, and holds what this section said about the
joins under § Client-target joins and about the flip under § ACME
convergence and § The flip orchestrator. "This document" and the sections
the text names with § are doctor.md's; "the plan" in its last paragraph is
the ACME flip plan.

## MVP scope

**In D2–D5:** derived catalog, config-root resolution, platform
inspectors, the check list including the no-SDK hardware family, the
`rusty-photon-doctor-checks` crate (now also the schema's canonical home
and the shared service-doctor runner), the per-service `doctor`
subcommands on all catalog services, aggregation over both probe paths,
text + `--json` rendering, exit codes, and `--fix`. Hardware probes never
open a device; SDK checks enumerate only; the aggregation probe against
local services stays on the host.

**In D6a (this document's §Provisioning):** the TLS + credential lifecycle —
self-signed issuance and ACME moved from `rp` (which loses `init-tls` and
`hash-password`), the pki tree under the config root, the minted observatory
credential and its distribution, `tls`/`auth`-on written by `--fix`, and the
`tls`/`auth` subcommands. Writes cover config files and the pki tree; ACME
is the one place doctor's network I/O leaves the host, and only when asked.

**In D6b (§Renewal):** `doctor tls renew` (both legs, hooks, retry), the
`tls.expiry` check, the ACME endpoint/trust/propagation knobs on
`tls issue --acme`, and the in-process cert hot-reload in
`rusty-photon-tls` — closing
[#541](https://github.com/rusty-photon/rusty-photon/issues/541).

**In D7 (§Renewal, §The derived catalog):** the packaging — doctor and
the renewal scheduling ship in sentinel's deb/rpm/MSI Core/brew formula,
the install-flow docs in the per-platform packaging guides, and the
pki-ownership alignment under sudo (the #572 remainder).

**Client-target joins ([#607](https://github.com/rusty-photon/rusty-photon/issues/607),
[#620](https://github.com/rusty-photon/rusty-photon/issues/620),
[#800](https://github.com/rusty-photon/rusty-photon/issues/800),
[#801](https://github.com/rusty-photon/rusty-photon/issues/801),
[#805](https://github.com/rusty-photon/rusty-photon/issues/805) gap 2):**
`joins.client-transport` and `joins.client-auth` (§Diagnosis), driven
from the one client-target registry, with the ACME name join and the
loopback→`<svc>.<domain>` host rewrite on an ACME install.
`joins.client-transport` is fully fix-eligible for ui-htmx's
`rp` target, sentinel's per-monitor `scheme`, sentinel's
`operation_watchdog.rp_url` scheme, the shared top-level `ca_cert` both
sentinel client families trust (#801), and rp's plate-solver/guider/plugin
clients (rp's shared top-level `ca_cert`, closed by #609/PR #612).
`joins.client-auth`
is fully fix-eligible for ui-htmx's targets, sentinel's per-monitor
`auth`, and (since #620 added `plate_solver.auth` /
`equipment.mount.guiding.auth`, and #800 `plugins[].auth`, to rp's
config schema) rp's plate-solver/guider/plugin clients; it does not run at all for
`operation_watchdog.rp_url` (its credential is the shared `service_auth`
pair — `auth.mismatch` already owns it).

**ACME convergence ([#805](https://github.com/rusty-photon/rusty-photon/issues/805)
gaps 1, 3, 4, 5, 6):** the five convergence checks (§Diagnosis §ACME
convergence) — `tls.stale-selfsigned-pointer`, `tls.stale-ca-pin`,
`sentinel.probe-domain`, `rp.advertised-url`, `dns.unresolvable` — grade
an already-provisioned install against the ACME target state `acme.json`
declares and make `--fix` converge everything provably doctor-owned, so
the flip recipe is `tls issue --acme` + `doctor --fix` + the reported
hosts entries + service restarts. The `flip-to-acme` orchestrator
(§The flip orchestrator, slice 4 of the plan) wraps that recipe in one
transaction: preconditions, issuance-or-accept, the in-memory staged op
plan, and the hosts-line verification, with `--dry-run` throughout.

Nothing in the MVP scope is deferred: every device-bearing service now
declares the USB identity its hardware reports (§The derived catalog).

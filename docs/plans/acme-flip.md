# Plan: one-command self-signed → ACME flip (#805)

## Goal

Flipping an already-provisioned self-signed install to publicly-trusted
ACME certificates is today a ~10-step manual operation
([#805](https://github.com/rusty-photon/rusty-photon/issues/805), written
from the real Pi5 rig flip). Doctor owns both end states — the
self-signed provisioning pass and `tls issue --acme` — but nothing owns
the **transition**. At the end of this plan:

```sh
doctor tls issue --acme --domain pier1.example.com --dns-provider cloudflare \
    --dns-token-var CLOUDFLARE_API_TOKEN --email me@example.com
doctor --fix          # converges the fleet onto the wildcard pair
# paste the /etc/hosts block doctor reports, restart services
```

is a working flip — and `doctor tls flip-to-acme` wraps the whole
sequence in one transaction with `--dry-run`.

## Background

The eight gaps, from #805 (numbering kept):

1. `--fix` plans zero ops on an already-provisioned fleet — the
   create-if-absent rule (`provision/mod.rs` `plan_service_client_wiring`,
   `fix::upsert`) never repoints a present `server.tls` at the wildcard
   pair.
2. Client target URLs are never host-rewritten. The wildcard's only SAN
   is `*.<domain>`, so every `https://127.0.0.1:<port>` client URL fails
   hostname verification. Worse than a no-op: `joins.client-transport`
   rewrites the *scheme*, reports `FIXED`, and leaves the host broken.
3. A stale `ca_cert` is never removed — and
   `rusty_photon_tls::client::client_builder` uses `tls_certs_only`,
   which **replaces** the platform roots, so a surviving pin rejects the
   publicly-trusted wildcard outright.
4. `sentinel.probe_domain` is never written (doctor already derives the
   identical value in `aggregate::acme_probe_domain`).
5. `rp.server.advertised_url` is never written (`https://rp.<domain>:<port>`
   is equally derivable).
6. Nothing checks that `<svc>.<domain>` resolves on the box; the robust
   answer for a night-time observatory is loopback `/etc/hosts` entries,
   which doctor should report and verify, not write.
7. `--config-dir` must pre-exist, blocking the recommended staging
   rehearsal into a scratch directory.
8. `--dns-token` persists the literal secret into `acme.json` by default
   (shell `$VAR` expansion), which also silently disconnects the
   `renew.env` rotation path.

Machinery already in place that this plan builds on:

- `fix::apply_ops(dir, ops, overwrite)` — `doctor auth rotate` is the
  precedent for a deliberate overwriting command built from hand-planned
  ops; the create-if-absent contract needs no change.
- `aggregate::acme_probe_domain` / `probe_host` — the `<svc>.<domain>`
  derivation, written and tested.
- `provision::acme_tls_block_value` — the `{cert,key}` block pointing at
  the wildcard pair; returns `None` unless both halves exist, which is
  the flip's precondition test.
- `checks::rewrite_scheme` — the byte-preserving URL rewrite a
  `rewrite_host` sibling follows.
- The `--fix` fixpoint loop (`diagnose_and_fix`, `MAX_FIX_ROUNDS`) —
  server-side fixes in round 0, dependent client-side fixes in round 1,
  no special-casing.

Constraints the design must respect:

- `resolve_join_target` joins only loopback hosts, so a host rewrite
  switches off every `joins.*` check unless the resolver learns the ACME
  name shape first.
- `target_uses_acme_cert` and `expiry_window_days` key on the literal
  file name `acme-cert.pem`; the flip writes exactly the
  `acme_tls_block_value` paths.
- The client-URL field surface is hand-wired across six sites in
  `checks.rs`/`scan.rs` (ui-htmx, sentinel ×2, rp ×3); a host rewrite
  wants one registry, not six edits.
- `apply_ops` aborts at the first unwritable file with earlier files
  already rewritten; "one transaction" means staging every mutated file
  in memory before the first write.

## Decisions

### D1 — Convergence-first: `acme.json` present means the fleet's target state is ACME

[#616](https://github.com/rusty-photon/rusty-photon/issues/616) already
established this for *new* services (an ACME install must not wire a new
service self-signed, because `tls_certs_only` clients cannot verify both
trust models at once). D1 extends the same contract to *existing*
services: once `acme.json` exists, a `server.tls` still pointing at
doctor's own self-signed pair, a lingering `ca_cert` pin, or a loopback
client URL against an ACME target is **divergence from the install's
declared state**, graded by checks and converged by `--fix`. The flip
becomes `tls issue --acme` + `doctor --fix`; the dedicated subcommand
(D6) is orchestration on top, not the only path. This also makes a
*partially* flipped install diagnosable forever — including one flipped
by hand before this plan landed — instead of only during the one command
run.

### D2 — Doctor overwrites only what it can prove is its own material

The provisioning contract ("present blocks are operator intent, never
overwritten") survives intact. The flip checks overwrite a present value
only when it is doctor-derived:

- a `server.tls` whose cert/key paths are the doctor-issued
  `pki/<svc>.pem` / `pki/<svc>-key.pem` under the resolved config root
  → repointed at the wildcard pair;
- a `ca_cert` / `ca_cert_path` pointing at `pki/ca.pem` under the
  resolved config root → removed (and any `ca_cert` on an ACME install
  is graded `fail` by `tls.stale-ca-pin`, since it disables the platform
  roots — but only the doctor-written path is fix-eligible);
- a client URL host that is loopback (`127.0.0.1`, `localhost`, `::1`)
  → rewritten to `<svc>.<domain>`, preserving scheme, port, path and
  query byte-for-byte.

A hand-placed foreign cert path or a non-loopback host stays
suggestion-only: doctor reports the divergence and the derivable value,
and the operator decides.

### D3 — The join resolver learns the ACME name shape

`resolve_join_target` additionally joins a URL whose host is exactly
`<candidate>.<domain>` — `domain` read from `acme.json`, `<candidate>`
resolved by the same effective-port match as the loopback path. Without
this, the flip permanently blinds `joins.client-transport` /
`joins.client-auth` (the #607 family), and the `--fix` fixpoint loop
could not even verify its own host rewrites converged. An unreadable
`acme.json` keeps the loopback-only shape, mirroring
`aggregate::acme_probe_domain`.

### D4 — Staging certificates never converge the fleet

An `acme.json` with `staging: true` in the real config root would flip
every client onto a publicly-**untrusted** certificate. The convergence
checks stay suggestion-only in that case (grading `warn`, naming the
staging state), and `flip-to-acme` refuses outright without an explicit
override flag. The staging rehearsal belongs in a scratch
`--config-dir`, which slice 1 makes frictionless.

### D5 — Hosts entries are reported and verified, never written

Doctor's write surface is config files plus the pki tree; `/etc/hosts`
stays outside it. A `dns.unresolvable` check resolves every derived
`<svc>.<domain>` the install depends on and fails with the exact hosts
line to paste (the loopback form — public DNS alone would make on-box
traffic depend on WAN and the DHCP lease, against tenets 1 and 2).
`flip-to-acme` runs the same verification before declaring success.

### D6 — `flip-to-acme` stages everything in memory, then writes

The subcommand's own op application reads every affected config into
memory, applies all ops, and only then writes file-by-file through
`rusty_photon_config::save` — so a planning failure costs zero writes. A
mid-write failure (unwritable file) still leaves earlier files rewritten
(POSIX offers no multi-file atomicity); the report names exactly what
was and wasn't written, and re-running either the command or `--fix`
converges the remainder (D1 is the backstop). `--dry-run` prints the op
plan (`FixOp: Display` already renders it) and writes nothing.

### D7 — `--dns-token-var` makes the safe token form the easy one

A new `--dns-token-var NAME` persists `$NAME` into `acme.json` — the
form `renew.env` rotation depends on — and cannot be destroyed by shell
expansion. `--dns-token` stays (a literal token is legal), but a value
not starting with `$` gets a loud stderr warning naming the consequence.
The two flags conflict.

### D8 — Rollback stays config-pointer-only, out of scope

The flip never deletes self-signed material, so rolling back is editing
config pointers back — manual today. A reverse `flip-to-selfsigned` is
future work if it ever earns its keep; it is not part of this plan.

## Slices

Each slice is an independently shippable PR; 1–3 each deliver value
alone, 4 is UX on top.

### Slice 1 — quick wins (gaps 7 + 8)

- `doctor tls issue` creates a missing explicit `--config-dir`
  (`create_dir_all`) instead of rejecting it — issuance is the one
  command whose job is materializing a tree from nothing, and the ACME
  path already `create_dir_all`s the pki tree underneath. Every other
  entry point keeps the rejection: a typo'd `--config-dir` on a
  diagnosis, `--fix`, or renewal run must not silently operate on an
  empty directory.
- `--dns-token-var` per D7, with the literal-token warning and the
  either-flag-required error.
- BDD scenarios in `acme_setup.feature` / `tls_issue.feature`; unit
  tests for the token-form helper.

### Slice 2 — the join family survives the flip (gap 2's foundation)

- One client-target registry replacing the six hand-wired URL/CA/auth
  pointer sites (pure refactor, no behavior change, unit-verified
  against the current check output).
- D3: `resolve_join_target` joins `<svc>.<domain>` hosts on an ACME
  install.
- `joins.client-transport` on an ACME install plans the host rewrite
  (loopback → `<svc>.<domain>`, D2) alongside its existing scheme
  rewrite — closing gap 2 and the false-`FIXED` defect from #805's
  correction comment. `rewrite_host` sibling of `rewrite_scheme`,
  byte-preserving.

### Slice 3 — convergence checks (gaps 1, 3, 4, 5, 6)

All gated on `acme_active()` and D4's staging guard; all fix-eligible
per D2's provenance rule, suggestion-only otherwise:

- `tls.stale-selfsigned-pointer` — `server.tls` points at the
  doctor-issued per-service pair while the wildcard pair exists → repoint
  (fix `fail`; the flip case where overwriting is exactly intended).
- `tls.stale-ca-pin` — `ca_cert` / `ca_cert_path` present on an ACME
  install → `fail` (it disables the platform roots); doctor-written
  `pki/ca.pem` paths get a `remove-key` fix.
- `sentinel.probe-domain` — absent while `acme_active()` → fix writes
  `acme.json`'s `domain` (needs `probe_domain` surfaced in
  `SentinelView`).
- `rp.advertised-url` — absent while `acme_active()` → fix writes
  `https://rp.<domain>:<port>` (needs `advertised_url` surfaced through
  the scan; `config.core()` drops it today).
- `dns.unresolvable` — D5's resolution check, report-only.

After this slice the flip recipe is issue + `--fix` + hosts + restart.

### Slice 4 — `doctor tls flip-to-acme`

The orchestrator: run issuance (or accept the existing pair),
preconditions (every installed service has a config file; wildcard pair
present or issuance succeeded; D4's staging refusal), derive the full op
plan from the slice-3 planning functions, apply per D6, then report the
hosts block and verify resolution. `--dry-run` throughout. Reuses
`AcmeArgs` including `--dns-token-var`.

## Verification

- Slice 1: BDD — missing `--config-dir` created by `tls issue`, still
  rejected by the default run; `$NAME` persisted by `--dns-token-var`
  with the run failing later at credential resolution (proving the file
  wrote first); literal `--dns-token` warns; `$`-form does not; the two
  flags conflict. Unit — the token-form helper's full matrix.
- Slice 2: unit — registry output identical to the six current sites
  for a representative config set; `rewrite_host` never introduces a
  trailing slash (the `rewrite_scheme` precedent); resolver joins
  `<svc>.<domain>` only with a readable `acme.json` and an exact-name +
  port match. BDD — a `client_target_joins.feature` ACME scenario where
  the fixed URL verifies against the wildcard name.
- Slice 3: BDD per check (staged `acme.json` + wildcard pair fixtures
  exist in `provisioning.feature` already): fires on divergence,
  fix converges within the fixpoint loop, second `--fix` applies
  nothing, staging `acme.json` downgrades to suggestion-only, hand-set
  material stays untouched.
- Slice 4: BDD — `--dry-run` writes nothing and prints the plan;
  preconditions each refuse with a named reason; a full flip against a
  staged self-signed tree converges to the same state slice 3's `--fix`
  reaches; idempotent re-run. On-rig: nothing to flip (the rig already
  runs ACME) — a `--dry-run` there must plan zero ops, which is itself
  the validation.

## References

- [#805](https://github.com/rusty-photon/rusty-photon/issues/805) — the
  gap list and correction comment
- [doctor.md](../services/doctor.md) — §Provisioning, §What `--fix`
  adds, §Client-target joins
- [#616](https://github.com/rusty-photon/rusty-photon/issues/616) —
  ACME-aware provisioning (D1's precedent), [#607](https://github.com/rusty-photon/rusty-photon/issues/607) /
  [#614](https://github.com/rusty-photon/rusty-photon/issues/614) — the
  join checks and `probe_domain`
- [#820](https://github.com/rusty-photon/rusty-photon/issues/820) — the
  probe-host + trust-root pairing the resolver extension mirrors

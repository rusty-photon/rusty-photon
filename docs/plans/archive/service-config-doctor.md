# Service Config Doctor Plan — installer puts bytes, doctor wires configs

**Status: COMPLETE (archived 2026-07-24).** All eight phases shipped to `main`:
D0 #539, D1 #549, D2 #554, D3 #560, D3s #559, D4 #563, D5 #568, D6 #564 + #573,
D7 [#589](https://github.com/rusty-photon/rusty-photon/pull/589) (merged
2026-07-19). The on-rig verification leg ran after D7 and its field findings
landed as follow-up fixes — notably
[#602](https://github.com/rusty-photon/rusty-photon/pull/602) (the
`hardware.serial-access` check now models the kernel's group union, after a
false FAIL on a healthy rig), plus ACME/provisioning hardening (#616/#646,
#618, #623, #640), credential and CA wiring (#612, #649, #652, #655, #656),
and catalog registrations for late-packaged services (#661/#692 session-runner,
#678/#687 svbony-camera).

Deliberately parked, tracked separately:
[#582](https://github.com/rusty-photon/rusty-photon/issues/582) — re-enable the
MSI nightly-over-nightly upgrade proof, redesigned around the now-shipped
`doctor --fix`; and
[#229](https://github.com/rusty-photon/rusty-photon/issues/229) — the
`cloudflare`/`reqwest` duplication, whose blast radius D6 reduced from the
workspace to doctor as promised (`install_default_crypto_provider` survives at
`crates/rusty-photon-tls/src/lib.rs:27` as the belt-and-suspenders this plan
said it would). The "Future considerations" section below is unstarted by
design — it was never in scope.

## Goal

Make a multi-service rusty-photon install coherent without hand-wiring. Today
an operator provisioning a rig hand-edits up to 14 JSON files that share no
schema fragment, and types the same port, URL, credential, and service name
into two or three of them. Nothing validates the cross-references: a mismatched
name surfaces at 2am as a 404 in a UI banner.

The outcome: packages put bytes on disk; a standalone `rusty-photon-doctor`
binary diagnoses and repairs the configuration afterwards. Doctor owns
**service** facts (ports, TLS, auth, service-to-service references, hardware
reachability). It never learns what a camera is *for* — device usage stays in
`rp`.

This plan also collapses the 13 independent `ServerConfig` definitions into
one shared crate, `rusty-photon-server-config`. That is not a side quest: it
is what lets doctor read the `server` block out of any `<svc>.json` while
treating the other 95% of the file as opaque bytes, and therefore what keeps
doctor *out* of the services rather than a component of them.

## Implementation Status

| Phase | Description | Status | Branch / PR |
|-------|-------------|--------|-------------|
| D0 | This plan + [ADR-016](../../decisions/016-service-config-ownership-and-doctor.md) (config ownership + the SDK line) | Merged | #539 |
| D1 | `rusty-photon-server-config` (core + Alpaca shapes); all 18 services adopt; TLS/auth for the 9 that lack it; `bind_address` everywhere (default `0.0.0.0`); per-service TLS+auth smoke scenarios | Merged | #549 (follow-up: [#550](https://github.com/rusty-photon/rusty-photon/issues/550), smoke-fixture dedupe) |
| D2 | `rusty-photon-doctor` binary: catalog + service-config diagnosis (read-only) | Merged | #554 |
| D3 | `--fix`; ui-htmx sources from rp's roster (its `drivers` map becomes an empty-by-default override; the map was later deleted entirely — [#569](https://github.com/rusty-photon/rusty-photon/issues/569), ADR-016 amendment 6) | Merged | #560 → #559 |
| D3s | Sentinel discovers its services; delete the `services` map; policy → constants (privilege path shipped — polkit rule in the sentinel packages) | Merged | #559 |
| D4 | `rusty-photon-doctor-checks` crate + generic hardware checks (no SDK) | Merged | [#563](https://github.com/rusty-photon/rusty-photon/pull/563) |
| D5 | Per-service `doctor` subcommand + aggregation | Merged | [#568](https://github.com/rusty-photon/rusty-photon/pull/568) |
| D6 | Move the TLS + credential lifecycle `rp` → doctor; split `rp-tls`; certs to `~/.config/rusty-photon/pki`; doctor generates certs + mints one credential + writes TLS-on/auth-on config | Merged | D6a [#564](https://github.com/rusty-photon/rusty-photon/pull/564); D6b [#573](https://github.com/rusty-photon/rusty-photon/pull/573) |
| D7 | Packaging (doctor + renewal timers ride sentinel's artifacts), install-flow docs, on-rig verification | Merged | [#589](https://github.com/rusty-photon/rusty-photon/pull/589) (on-rig follow-up: [#602](https://github.com/rusty-photon/rusty-photon/pull/602); MSI upgrade proof: [#582](https://github.com/rusty-photon/rusty-photon/issues/582)) |

## Decisions (fixed — see [ADR-016](../../decisions/016-service-config-ownership-and-doctor.md) for rationale)

1. **The installer puts bytes on disk. Doctor wires the configs.** Packages do
   not generate or seed config; postinst does not call doctor. Services
   self-create their defaults as they already do, and the operator runs
   `rusty-photon-doctor --fix` once to make the install coherent.

   This is what keeps the design tractable. Generation inside postinst would
   have to converge across N package installs in arbitrary order — the wart
   the MSI's seed-once already has, documented at `docs/packaging-windows.md:182`
   (*"after adding features to an existing install, add the new service's
   entry by hand"*). An operator-run doctor sees the whole system at once and
   converges in a single pass. No ordering problem, no idempotent merge, and no
   `Depends:` inversion putting doctor under all 17 packages.

2. **One config file per service. Not a joint file.** A single
   `rusty-photon.json` would kill the duplication by construction, but every
   service with `config.apply` writes its config back; concurrent atomic
   renames to one shared file means lost updates. The per-file model is what
   makes those self-rewrites safe. The correctness burden moves onto doctor
   instead.

3. **Doctor is a standalone binary, not a component of the services.** It links
   no service crate. It knows the catalog, the two shared `ServerConfig` shapes
   (core and Alpaca — see D1), and the known cross-reference blocks (sentinel's
   `operation_watchdog`, rp's `equipment`/`session`); everything else in
   every config file is opaque `serde_json::Value` it steps around.

4. **Doctor's scope is service config, never device usage.** "Is `/dev/ttyUSB0`
   writable" is service health and in scope. "Which camera is the guide cam",
   dark-library setpoints, focal length, and UniqueID binding are device usage
   — `rp` owns those, and doctor never needs to know a serial exists.

5. **Hardware checks split at the SDK line.** Central doctor does everything
   that needs no vendor blob. Services own everything that does. This is forced
   by [ADR-014](../../decisions/014-zwo-per-device-services-and-link-features.md),
   not a judgment call — see Design below.

6. **The similarity lives in a shared library, not a shared binary.** Serial
   and USB reachability checks are near-identical across services. They go in a
   crate that per-service doctors call, so adding service #201 means
   implementing a small trait, not editing a central binary.

7. **The doctor report schema parses permissively.** `#[serde(default)]`,
   tolerate unknown fields — the opposite of the `deny_unknown_fields`
   convention every config uses. A config typo should be fatal; a doctor and a
   service from different nightly builds should degrade to a partial report,
   not refuse to run.

8. **Sentinel discovers its supervised services; its `services` map is
   deleted, not doctor-generated.** Sentinel shells out to `systemctl`, so it
   is same-host-bound by definition and can ask the platform — which, unlike a
   driver, is alive when the driver is dead. All policy in that map becomes
   constants (restart budget 300s; health poll 30s; threshold 3; backoff 60s
   doubling to 900s), health supervision becomes universal, and "not
   restartable" is removed: every service must come back when sentinel says so.

   The rule this follows is not "static vs. dynamic" but **whether the source
   of truth can be down when you need it.** Asking a driver how to restart
   itself fails that test; asking the service manager passes it.

## Design

### Ownership model

Three kinds of fact, one owner each:

| Kind | Examples | Owner |
|---|---|---|
| Service | port, bind, discovery, TLS cert, auth, unit name, restart command, serial/USB path | the service's own `<svc>.json`; **doctor** audits and repairs |
| Device | what hardware exists, its stable identity, its capabilities | the hardware, surfaced by the driver over `/management/v1/configureddevices` |
| Usage | roles ("guide cam"), dark-library setpoints, gain/offset, focal length | `rp.json`'s `equipment` block |

The drivers already implement this split correctly on their side.
`services/qhy-camera/src/config.rs:1-9` states it outright: *"The hardware is
the source of truth: the service enumerates every connected QHY camera … Config
therefore carries no per-camera binding — only optional per-serial display
overrides."* `DeviceOverride` across qhy-camera, zwo-camera, and zwo-focuser
carries nothing but `name`/`description`/`filter_names`. The gap is entirely on
the consumer side.

### D1 — the shared `ServerConfig`

There are 13 independent definitions today, seven of them field-for-field
identical:

| Definition | Shape |
|---|---|
| `services/dsd-fp2/src/config.rs:46` | port, discovery_port, tls, auth |
| `services/pa-scops-oag/src/config.rs:45` | *(identical)* |
| `services/qhy-focuser/src/config.rs:45` | *(identical)* |
| `services/pa-falcon-rotator/src/config.rs:43` | *(identical)* |
| `services/ppba-driver/src/config.rs:37` | *(identical)* |
| `services/filemonitor/src/lib.rs:82` | *(identical)* |
| `services/star-adventurer-gti/src/config.rs:92` | *(identical)* |
| `services/qhy-camera/src/config.rs:56` | port, discovery_port — **no tls/auth** |
| `services/sky-survey-camera/src/config.rs:142` | port, discovery_port — **no tls/auth** |
| `services/zwo-camera/src/config.rs:50` | port, discovery_port — **no tls/auth** |
| `services/zwo-focuser/src/config.rs:43` | port, discovery_port — **no tls/auth** |
| `services/rp/src/config/server.rs:6` | port, **bind_address**, tls, auth — no discovery_port |
| `services/ui-htmx/src/config.rs:40` | **bind**, port — no tls/auth/discovery |

Extract one crate, **`rusty-photon-server-config`** (workspace-infra naming,
like `rusty-photon-config` and `rusty-photon-shared-transport`). It depends on
`rp-tls` and `rp-auth` for the embedded types, and has no dependency in either
direction with `rusty-photon-config` — the machinery crate stays
`serde_json::Value`-based and shape-agnostic; this crate is the one typed
shape. Doctor is essentially the composition of the two plus the catalog.

The crate carries **two shapes**, both `deny_unknown_fields`:

- `ServerConfig` — `port`, `bind_address`, `tls`, `auth` — for the non-Alpaca
  services (rp, ui-htmx, sentinel, plate-solver, session-runner,
  calibrator-flats, phd2-guider).
- `AlpacaServerConfig` — the same plus `discovery_port` — for the 11 Alpaca
  drivers.

Two shapes rather than one because `discovery_port` on a non-Alpaca service
would be an accepted-but-inert knob — the exact silent-footgun class
`deny_unknown_fields` exists to prevent. The Alpaca shape declares all five
fields explicitly rather than `#[serde(flatten)]`-ing the core (serde's
`deny_unknown_fields` does not compose with `flatten`); a common-subset
accessor gives doctor and sentinel one view of `port`/`bind_address`/`tls`/
`auth` across both.

**Scope: all 18 services, one PR.** The 13 definitions above collapse into the
shared shapes, and the five services with ad-hoc listener configs (sentinel,
plate-solver, session-runner, calibrator-flats, phd2-guider) convert to
`ServerConfig` in the same pass. That makes D6's premise — every service has
`tls`/`auth` fields — true immediately after D1, with no straggler phase.

Consequences:

- **Nine services gain TLS and auth.** The five from the table (qhy-camera,
  zwo-camera, zwo-focuser, sky-survey-camera, ui-htmx) plus plate-solver,
  session-runner, calibrator-flats, and phd2-guider. sentinel's dashboard
  already carries `tls`/`auth` (`services/sentinel/src/config.rs:336-349`)
  and converts shape-only. sky-survey-camera is the sharpest case: it
  consumes `ClientAuthConfig` outbound (`config.rs:90,114`) but exposes no
  inbound auth. Every service ends up with a **top-level `server` block** —
  that placement is the doctor contract (sentinel's moves out of
  `dashboard`, calibrator-flats gains one where port/bind were CLI-only).

  **Absent `tls`/`auth` still means plain, unauthenticated HTTP.** Both are
  turned on in D6 via doctor's *generated config*, not via the serde default —
  see ADR-016 decision 10(d) for why that distinction is load-bearing.

  This **supersedes [#524](https://github.com/rusty-photon/rusty-photon/issues/524)**
  and adopts both its transport and auth halves. Its premise was false: it
  assumes every service has a `tls` knob whose default needs flipping, but for
  these four Alpaca drivers the field is **absent**, and #524 names only ui-htmx
  as lacking support.
- **The bind-address naming split resolves.** Every service gains a
  configurable `bind_address` with a **unified default of `0.0.0.0`**,
  replacing `bind_address` (rp, plate-solver, phd2-guider — default
  `127.0.0.1`), `bind` (ui-htmx, default `127.0.0.1`), CLI-only bind flags
  (session-runner, calibrator-flats — default `127.0.0.1`), and
  absent-and-hardcoded-`0.0.0.0` (the 11 Alpaca drivers and sentinel). This
  is D1's **one deliberate behaviour change**: six services (rp, ui-htmx,
  plate-solver, session-runner, calibrator-flats, phd2-guider) move from a
  loopback default to LAN-exposed. Existing installs whose config files carry
  explicit values are unaffected; ones that relied on the old defaults (rp's
  scaffold wrote `"server": {}`) pick up the new default — or, where the
  schema changed shape (`port` is now required when the block is present;
  ui-htmx's `bind` rename), fail loudly at next start and need a one-line
  edit. The interim exposure is accepted because D6 makes TLS + auth the
  default for every real deployment. Default *ports* stay per-service,
  supplied by each service's parent-config constructor, not by serde defaults
  inside the shared shapes.
- **Doctor becomes possible.** One crate to parse the `server` block out of
  all 18 files.

`discovery_port` (Alpaca shape only) keeps its
`#[serde(default, skip_serializing_if = "Option::is_none")]`
so `config.apply` cannot re-persist a stale key, and its default stays `None` —
the collision rationale at `crates/rusty-photon-driver/src/discovery.rs:12` is
unchanged by this plan.

Pre-1.0 breaking config-schema changes are sanctioned, so the rename of
`ui-htmx`'s `bind` → `bind_address` needs no migration shim.

**Verification for D1** (in addition to the crate's unit tests): every service
gets one TLS+auth **smoke scenario** — boots with TLS and auth configured,
rejects an unauthenticated request, answers an authenticated HTTPS one —
proving each service actually threads the shared config into its serve path.
ui-htmx gets a **full mirrored auth/TLS suite** (its axum stack is the one
with no existing pattern); ppba-driver's existing `auth.feature` remains the
deep representative suite for the shared Alpaca driver stack.

### D2 — the catalog and service-config diagnosis

The full D2 specification is the design doc,
[`docs/services/doctor.md`](../../services/doctor.md). Settled interactively
2026-07-16 (the eight open choices, recorded here so later phases inherit
them):

1. **Location** — `services/doctor` (cargo binary `doctor`, installed as
   `rusty-photon-doctor` since D7). It never gets a `pkg/` dir: the
   packaging rides entirely in sentinel's artifacts (decision 8 below), so
   doctor stays out of its own catalog and the `services/*/pkg` discovery
   loops — the unit-less one-shot carve-out turned out to be "no
   enrollment at all", not a relaxed contract.
2. **Catalog mechanism** — per-service metadata files,
   `services/<svc>/pkg/doctor.toml` (`class = "alpaca"|"core"`, `port`).
   Each service unit-tests its own file against its own config defaults;
   doctor embeds the files at build time; CI asserts every `services/*/pkg`
   dir carries one. The packaged set was **17** services when D2 landed;
   session-runner and svbony-camera self-enrolled as their packaging landed
   (#661/#692, #678/#687), so it is **19** as of archiving.
3. **Installed-detection** — the service manager knows a `rusty-photon-*`
   unit, OR a config file exists (covers dev checkouts). Matches D3s's
   discovery exactly.
4. **Platforms** — all three inspectors (systemd, Windows SCM, brew
   services) fully implemented and host-validated in the D2 PR, not
   deferred.
5. **Static only** — no network I/O in D2; liveness probes arrive with the
   phases that need them (D3/D5).
6. **CLI contract** — human text by default, `--json` emits the permissive
   `DoctorReport` schema from day one; exit 0 = no failures, 1 = failures
   found, 2 = doctor could not run.
7. **Config root** — `--config-dir` > the packaged `/etc/rusty-photon`
   symlink (Unix) > the platform default the services themselves resolve.
8. **Parse scope** — JSON syntax + the catalog-declared `server` shape + the
   known cross-reference blocks (ui-htmx `sentinel`, sentinel
   `services`/`operation_watchdog`, rp `equipment[].alpaca_url`/`session`;
   ui-htmx's `drivers` map left the list when #569 retired it).
   Full-shape typo detection is honestly deferred to D5's per-service
   subcommands.

**The catalog must be derived, not typed.** `services/<svc>/pkg` existing is
already the packaging authority, and both packaging scripts derive the service
list from it with a byte-identical line —
`scripts/build-packages.sh:111` and `scripts/generate-brew-formulas.sh:94`:

```sh
ALL_SERVICES=$(for d in services/*/pkg; do [ -d "$d" ] && basename "$(dirname "$d")"; done | tr '\n' ' ')
```

Doctor's catalog (service name, default port, unit name per platform) comes
from the same place, via the `doctor.toml` mechanism above, with CI asserting
the table matches the tree.

This matters because `rusty-photon-<svc>` is *already* independently re-encoded
in `.service` files, `.wxs` fragments, `generate-brew-formulas.sh`, and
`scripts/rig.sh:39` — and the copies drifted so far that every documented
`restart_command` in the repo is wrong twice over (wrong scope: `--user`
against system units; wrong name: missing the `rusty-photon-` prefix), and one
still names `qhyccd-alpaca`, a dead predecessor project, pointed at
filemonitor's port. A hand-typed catalog would be the fifth encoding and would
rot the same way.

What D2 diagnoses — all service-level, zero device knowledge:

- **Port collisions** across the hand-allocated 11111–11170 range.
- **The sentinel privilege path.** The sentinel package ships the scoped
  polkit rule (`services/sentinel/pkg/50-rusty-photon-sentinel.rules` →
  `/usr/share/polkit-1/rules.d/`) that lets its `NoNewPrivileges=yes` unit
  restart `rusty-photon-*` units. Doctor verifies the rule is actually
  installed: on a host installed from packages predating the rule (or where
  it was hand-removed), sentinel is back to restarting nothing, and only a
  2am failure would surface that.
- **Dangling name joins.** ui-htmx `drivers` key → `sentinel_service` →
  sentinel `services` key → `operation_watchdog.operations.<family>.service`.
  Four spellings of one service name, matched by convention, unvalidated. D3s
  deletes the third; the survivors must name a unit the service manager
  actually reports.
- **Config-gated services that will never start.** sky-survey-camera,
  plate-solver, calibrator-flats, and phd2-guider hard-require a config file
  and carry `ConditionPathExists=`. Installed, enabled, silently inert.
- **Unparseable configs.** `deny_unknown_fields` makes a typo fatal at startup;
  doctor catches it before the next night.
- **TLS cert/key paths** absent or unreadable by the `rusty-photon` user.
- **Platform-wrong defaults**, e.g. rp self-creating `session.data_directory`
  as a Linux path that is not writable on macOS (`docs/packaging-macos.md:136`).
- **URL convention mismatches** — sentinel's `base_url` wants an `/api/v1`
  suffix; rp's `alpaca_url` and ui-htmx's `base_url` do not. D3s retires
  sentinel's side of this by deriving the URL, which removes the mismatch at
  its source rather than checking for it.

D2 is read-only. It reports and suggests; it writes nothing.

### D3 — `--fix`, and ui-htmx sourcing from rp

ui-htmx's source of truth is **rp's roster**, which it already derives at
runtime; the target keeps only that (ADR-016 decision 9). Its own config
shrinks to its listening port and where rp is (`localhost` by default on the
single box). The static `drivers` map survives only as an optional override —
a third-party device rp does not manage, or a driver given a separate
credential — empty for a stock rig, and `--fix` leaves it that way.

> **Tightened 2026-07-18** ([#569](https://github.com/rusty-photon/rusty-photon/issues/569),
> ADR-016 amendment 6): the override did not survive field review — the
> `drivers` map is deleted entirely, the `rp` target is required, a config
> still carrying the map fails loudly with the deletion in doctor's
> `config.retired-keys` catalog, and the restart affordance is derived by
> matching a roster device's `alpaca_url` port against sentinel's
> `GET /api/services` `probe_port`.

The liveness objection ("if rp is down the UI goes blind") dissolves against
the recovery model: when rp will not start you `ssh` in and run `doctor`, you
do not reach for the browser, so a blind UI during an rp outage costs nothing.
The three forces that used to require a static map — auth redaction, devices rp
could not model, restart wiring — are gone (doctor mints ui-htmx's credential
per D6, #534 adds the device kinds, D3s removes `sentinel_service`).

`rp.json`'s `equipment[].alpaca_url` is the operator-facing copy and stays
inside the device-usage block doctor does not cross. Doctor *checks* it (is the
port real, does a service listen there) but does not own it. The enemy is
**hand-maintained** duplication, not duplication on disk.

Sentinel is deliberately **not** in this phase — see D3s, where its map is
deleted rather than generated.

Write safety: `--fix` uses `rusty_photon_config::save`'s existing atomic
temp→fsync→rename→fsync-dir path, making doctor a third writer alongside
operator hand-edits and drivers' own `config.apply`. Atomic rename means no
corruption, only a possible lost update, and `--fix` is an
operator-initiated foreground action. Doctor must reuse the layer-aware persist
rules — it must not bake a transient CLI override into a file.

### D3s — sentinel discovers; the `services` map is deleted

Sentinel's map is not doctor-generated. It is **deleted**. A doctor-written map
goes stale the moment a package is installed and stays stale until someone
re-runs `--fix`; asking the platform costs nothing and is never stale. Sentinel
already shells out to `systemctl`, so it is **same-host-bound by definition**
and every fact in the map follows from that.

| Was | Becomes |
|---|---|
| the `services` map (which services to supervise) | enumerate `rusty-photon-*` from the service manager |
| `restart_command` | derived from the unit name |
| `health_command` | derived — `systemctl is-active <unit>` |
| `base_url` | read the service's own `<svc>.json` → `server.port` (shared shapes after D1) |
| `health.url` | derived from port + catalog class (below) |
| `max_restart_duration` | constant **300s** |
| `health.poll_interval` | constant **30s** |
| `health.failure_threshold` | constant **3** |
| `health.restart_backoff` / `_max` | constants **60s**, doubling to **900s** |
| `restart_command: null` ("not restartable") | **removed** |

Every constant except the restart budget is the shipped default promoted to a
constant (`services/sentinel/src/config.rs:411-430`), so behaviour is unchanged;
`max_restart_duration` moves 60s → 300s.

**The health probe URL is derivable because there are exactly two classes**, and
which class a service is in is a static catalog fact:

- **Alpaca drivers** answer `GET {base}/management/v1/configureddevices` — no
  device number needed, so no device knowledge leaks into sentinel.
- **Non-Alpaca services** (rp, plate-solver, session-runner, calibrator-flats,
  phd2-guider, ui-htmx) answer `GET {base}/health`. These are exactly the seven
  that define a `/health` route today; the Alpaca drivers have none, by design.

**Health supervision becomes universal.** Presence of a `health` block is
currently the opt-in; with the block gone, every discovered service is
supervised. That is the tenet-#2 answer and removes a footgun where forgetting
a block silently meant no supervision. Expect the first deployment to surface
flapping that was previously invisible.

**"Not restartable" is removed.** Every rusty-photon service must come back when
sentinel says so; one that cannot is a bug to fix, not a config to write. The
escape hatch's stated purpose — a remote MCU we cannot `systemctl` — is moot
under same-host discovery: such a device was never a local unit, so it is never
enumerated.

**This phase is gated on the privilege path** (Flagged unknowns). Today sentinel
runs `NoNewPrivileges=yes` with no polkit rule, so it can restart nothing. Once
supervision is universal and "not restartable" is gone, *every* service depends
on a privilege path that does not exist — which turns that open question from a
parallel concern into a prerequisite.

**This breaks merged code.** The per-service `health` block shipped in #505
(merged 2026-07-13). Deleting it is a breaking config-schema change, sanctioned
pre-1.0. That PR's settled choices survive the move: never-give-up backoff is
what the constants encode, and no-recovery-notification is untouched.

**D1 is a hard prerequisite**, not just for doctor: sentinel reading
`<svc>.json` for a port only works once every service's `server` block uses
the shared shapes.

### D6 — the TLS and credential lifecycle moves to doctor

Runs after D2 (doctor exists, catalog derived) and D1 (every service has `tls`
and `auth` fields). Per ADR-016 decision 10:

**Split `rp-tls`.** The serving half (`server`, `client`, `config`,
`permissions`, `error`) stays a dependency of all 18 services. The
provisioning half (`cert`, `acme`, `acme_config`, `dns`) goes to doctor. This
is the phase's main payoff beyond the feature: `rp-tls` today drags
`cloudflare` + `instant-acme` into **every service that only wants to serve
HTTPS**, and `install_default_crypto_provider` (`lib.rs:27`) exists solely
because of it — *"both `aws-lc-rs` and `ring` end up feature-activated on
rustls via our transitive deps (reqwest 0.13 + reqwest 0.12 via cloudflare
rustls-tls)"*. Quarantining `cloudflare` to one binary cuts
[#229](https://github.com/rusty-photon/rusty-photon/issues/229)'s blast radius
from the workspace to doctor, and may let the crypto-provider workaround go for
services entirely. Verify that claim by checking whether `ring` still gets
activated once `cloudflare` is out of a service's tree.

*Verified after the D6a split* (`cargo tree -p ppba-driver -i ring`):
`reqwest 0.12` and `cloudflare` are gone from every service tree (only
`reqwest 0.13` remains), and `ring` is **no longer feature-activated on
rustls** there — it survives only as `rcgen`/`x509-parser`'s internal
dependency via `rusty-photon-tls`'s `test_cert` module. So the provider
ambiguity the workaround guards against is gone for services;
`install_default_crypto_provider` stays as a harmless belt-and-suspenders
until a dedicated pass retires it (candidate for D6b or #229).

**Retire `DEFAULT_SERVICES`.** `rp_tls::cert::DEFAULT_SERVICES` lists five of
eighteen — the sixth hand-typed encoding of the service list, and stale enough
that dsd-fp2, pa-falcon-rotator, pa-scops-oag and star-adventurer-gti get no
cert despite *having* `tls` fields. Doctor's derived catalog replaces it.

**Move the paths.** `~/.rusty-photon/pki` → `~/.config/rusty-photon/pki`;
`acme.json` alongside the configs. One tree, covered by the existing
`/etc/rusty-photon` symlink. Under the packaged deployment that is
`/var/lib/rusty-photon/.config/rusty-photon/pki`, which `ReadWritePaths=` already
covers; keys stay 0600 and owned by `rusty-photon`.

**Move the commands.** `rp init-tls` is removed; doctor gains issuance, ACME,
and renewal. `rp`'s `acme_setup.feature` / `tls_setup.feature` and
`bdd-infra`'s one-shot command tests (`lib.rs:1199`) move with it.

**Credentials too.** D6 also mints one observatory credential and distributes
it (ADR-016 decision 10(e)): the Argon2id hash into each service's
`server.auth`, the plaintext into each client's auth block. `rp hash-password`
moves here from rp along with `init-tls`. Because doctor generates the
credential it holds the plaintext at mint time, so it writes every copy in the
right form — which is what makes the auth-on default machine-maintained rather
than a hand-typed sprawl.

Then `doctor --fix` generates certs + the credential and writes `tls` and
`auth` on for every service it wires. **Absent `tls`/`auth` still means plain,
unauthenticated HTTP** — see decision 10(d): packages start services at
install, before any doctor run, so a serde default of "on" would strand every
fresh install without certs and credentials, and would break every BDD and
ConformU test that hand-writes a config omitting them
(`services/ppba-driver/tests/conformu_integration.rs:79`).

Settled interactively 2026-07-17 (the eight open choices, recorded here so
the implementation and later phases inherit them):

1. **Slicing — two PRs.** D6a is the lifecycle move: the `rp-tls` split,
   `DEFAULT_SERVICES` retirement, the path move, the commands moving to
   doctor, and credential mint + TLS/auth-on in `--fix`. D6b is renewal —
   [#541](https://github.com/rusty-photon/rusty-photon/issues/541)'s scope
   (renewal command, expiry checking, the swap, ACME end-to-end tests).
   Each stays independently reviewable and the move is not hostage to
   ACME test infrastructure.
2. **Renewal scheduler — one-shot `doctor tls renew` on a platform
   scheduler** (systemd timer / Windows scheduled task / launchd interval —
   certbot's shape). No background task in any service: renewing
   zwo-camera's cert must not require rp (or anything else) to be running.
   The one-shot is a no-op unless a cert is within its renewal window, so
   the same timer serves self-signed (ten-year, effectively never) and
   ACME (90→45-day) installs. Timer units ship with the packaging (D7).
3. **The swap — in-process hot reload** (`ReloadableCertResolver`, ADR-002
   Phase 2), not restart-via-sentinel. Services pick up a renewed cert
   without restarting, so renewal can never interrupt an exposure at all —
   the mid-exposure hazard is removed rather than scheduled around.
4. **Reload trigger — throttled mtime re-check.** The resolver stats the
   cert file on handshake (cached, re-checked at most every ~60s) and
   reloads on mtime change. No new dependencies, no signals, no watcher
   lifecycle; portable to all three platforms. Lands with D6b alongside
   renewal.
5. **Auth rotation still restarts.** Hot reload covers certs only.
   `doctor auth rotate` is operator-initiated and rare, so services pick
   up a rotated `server.auth` by restart; no reload machinery grows into
   the auth middleware.
6. **Crate split — provisioning into doctor as modules; serving half
   renamed `rusty-photon-tls`.** `cert`/`acme`/`acme_config`/`dns` (and
   the `cloudflare` + `instant-acme` dependencies) move into
   `services/doctor` directly — doctor is their only consumer, so no new
   crate. The serving half (`server`, `client`, `config`, `permissions`,
   `error`) is renamed `rp-tls` → `rusty-photon-tls` in the same pass,
   matching the workspace-infra naming convention while the break is
   already sanctioned.
7. **Credential canonical copy — a 0600 file under the pki tree**
   (`~/.config/rusty-photon/pki/credential`, owned by the service user,
   beside the CA key it is morally equivalent to). `--fix` reuses it when
   wiring a newly installed service, recovery for a forgotten credential
   is reading it there, and `doctor auth rotate` overwrites it and
   re-runs distribution.
8. **Doctor ships in sentinel's package** (implemented in D7). Sentinel is the
   always-installed supervisor; its deb/rpm/MSI/brew artifact carries the
   doctor binary and the renewal timer units. No separate
   `rusty-photon-doctor` package, and no service package grows a doctor
   dependency — ADR-016 decision 1's operator-run model is unchanged;
   only the delivery vehicle is bundled.

### D4/D5 — hardware checks, and why the SDK line is not a judgment call

[ADR-014](../../decisions/014-zwo-per-device-services-and-link-features.md) exists
because of exactly the failure a central hardware-probing doctor would recreate:

> Installing `rusty-photon-zwo-camera` and `rusty-photon-zwo-focuser` together
> failed: both debs shipped **all three** ZWO SDK blobs at the identical path
> `/usr/lib/rusty-photon/`, and dpkg refuses two owners of one file. […] **Each
> package ships exactly its own blob.**

A doctor that *opens* hardware must link `libASICamera2` + `libEAFFocuser` +
`libqhyccd` + every future vendor blob, and therefore ship them all, at that
same path — reintroducing the file conflict ADR-014 was written to fix, and
forcing a QHY-only rig to install ZWO's SDK. That is a hard blocker, not a size
objection.

So:

**D4 — no SDK needed → central doctor**, via a shared
`rusty-photon-doctor-checks` crate (contract now specified in
[doctor.md §Hardware](../../services/doctor.md); settled choices: one severity
rule — fail when the unit is enabled, warn otherwise; device-presence
checks on all three platforms, the udev/group/firmware mechanics on Linux):

- device node exists at the configured serial path (catalog-declared JSON
  pointer + platform defaults; star-adventurer-gti gated on
  `transport.kind == "usb"`); openable by `rusty-photon`
- group access is judged the way it actually works: membership is conferred
  per-unit via `SupplementaryGroups=` (**never** `/etc/group` — no packaging
  step edits it), and `/etc/group` matters only for group *existence*, which
  is what makes udev's `GROUP=` resolvable
- the udev rule is installed **and parsed** — udev silently drops an entire
  rule line on an unresolvable `GROUP=`, so "the file is present" is not the
  check; installed content is also diffed against the packaged copy (warn on
  drift — `/etc/udev/rules.d` overrides are legitimate)
- USB device present, for every device-backed service: declared
  vendor(:product) plus a product-string substring where the VID:PID is a
  generic bridge chip — the three Pegasus devices all report FTDI's
  `0403:6015` and the FP2 reports the RP2040's `2e8a:000a`, so VID:PID alone
  cannot name the device (rig-measured 2026-07-17)
- the firmware helper has been run — the qhy helper's own idempotency gate,
  a three-artifact conjunction. `packaging/postinst.udev-stanza` already
  tells operators *"camera firmware is not bundled. Run '<helper>' once as
  root before first use"* (ADR-013 forbids packaging proprietary firmware)
  and nothing verifies they did.

The probe/predicate helpers live in `rusty-photon-doctor-checks` itself
rather than `rusty-photon-shared-transport`: the transport crate holds no
path or enumeration API (each driver's factory owns its path privately),
and doctor must not inherit a driver runtime dependency for a `stat`.

**D5 — SDK needed → the service**, via a `doctor` subcommand on each service
binary that links only its own blob, emitting the shared report schema as JSON.
The similarity is factored into the D4 crate, so a serial driver's `doctor` is
a handful of calls to shared helpers.

The precedent for this shape is already in the tree:
`packaging/postinst.udev-stanza` is **shared** shell,
`services/zwo-camera/pkg/90-rusty-photon-zwo.rules` is **per-service** data.

D5's settled choices (contract in [doctor.md §Per-service
doctors](../../services/doctor.md)):

- **All catalog services** carry the subcommand, not just the three SDK
  services — the subcommand's `config.full-shape` check is where the
  full-config typo detection D2 deferred actually materializes, and it
  needs the binary that owns the typed shape. The SDK trio (qhy-camera,
  zwo-camera, zwo-focuser) additionally enumerates.
- **Enumeration only, never an open.** The SDK check reports what the SDK
  sees (`ScanQHYCCD`, the ASI/EAF list calls); an open would recreate the
  camera-lock class of bug against a running service, and the subcommand
  must stay safe to run by hand at any time.
- **Both aggregation paths land in D5**: running Alpaca service → HTTP
  `configureddevices` probe (the phase's one network I/O — local
  services only); installed-but-stopped service → shell out to the
  binary's own `doctor --json`, parsed permissively, with version skew
  degrading to a warning.
- **The report schema re-homes** into `rusty-photon-doctor-checks` as its
  canonical location (services serialize it from D5 on); central doctor
  re-exports. Sequenced after D6a ([#564](https://github.com/rusty-photon/rusty-photon/pull/564)),
  which reworks the same doctor sources.
- Per-service checks plan **no fixes** — `--fix` stays a central-doctor
  concern, in one binary.

### The two probe paths are naturally exclusive

Central doctor needs no vendor blob on either path:

- **Service running** → it already enumerated its hardware at startup. Ask
  `/management/v1/configureddevices` over HTTP. No SDK, no hardware touch.
- **Service down** → shell out to `rusty-photon-<svc> doctor --json`. No lock
  contention, because nothing holds the device. This is also precisely the case
  being debugged.

This dodges a trap: **never open hardware a running service holds.** zwo-camera
Phase E already hit a camera-lock concurrency bug; a doctor that grabs the
camera mid-exposure to "check connectivity" is a tenet-#1 regression, and
reports a false "cannot reach hardware" the rest of the time.

### Report schema

One `DoctorReport { checks: Vec<Check> }`, where a `Check` carries a name,
status (`ok` / `warn` / `fail`), a human-readable detail, and an optional
machine-applicable `Fix`. Central doctor merges its own checks with each
installed service's, and renders one report. Per Decision 7, parsing is
permissive in both directions across the binary boundary.

## Verification

- **Unit** — catalog-matches-`services/*/pkg` test; per-check tests against
  tempdirs and fake sysfs/udev fixtures; report schema round-trip including a
  forward-compatibility case (unknown fields from a newer service).
- **BDD** — doctor against a scratch config dir: seed known-broken configs
  (port collision, dangling `sentinel_service`, unparseable JSON, missing
  `ConditionPathExists` file), assert the diagnosis, run `--fix`, assert
  convergence and idempotence on a second run.
- **On-rig (arm64 Debian test rig)** — the real proof: install the package set,
  run `--fix`, confirm a rig that previously needed hand-wiring comes up
  coherent. Then unplug hardware and confirm the report is honest.
- **Cross-platform** — the Windows VM (SCM service names, `%ProgramData%`
  paths) and macOS (brew formula names, the `session.data_directory` default).
  Doctor is explicitly an all-platforms tool, so all three legs are required.

## Flagged unknowns (resolve during the noted phase)

- **The sentinel privilege path — resolved
  ([#523](https://github.com/rusty-photon/rusty-photon/issues/523)); was the
  D3s blocker.** The sentinel deb/rpm ships the rig-verified scoped polkit
  rule at `/usr/share/polkit-1/rules.d/50-rusty-photon-sentinel.rules`
  (restart of `rusty-photon-*` units succeeds; non-prefixed units like
  `ssh.service` stay denied). It gates on
  `unit.indexOf("rusty-photon-") == 0 && verb == "restart"`, which lines up
  exactly with D3s enumerating `rusty-photon-*` — the rule's scope and the
  discovery scope are the same set, and the grant is live for every
  `rusty-photon-*` unit from install (it never depended on sentinel's
  `services` map), so D3s deleting that map changes nothing about it. The
  Windows analogue turned out to be a non-issue: the MSI installs every
  service — sentinel included — under `LocalSystem`, which may restart
  services; macOS `brew services` run as the operator's own user. What
  remains is verification, not decision: D2 checks the rule file is present
  on hosts installed from packages predating it.
- **Cert renewal does not exist — [#541](https://github.com/rusty-photon/rusty-photon/issues/541),
  and D6 re-homes it.** ADR-002 documents renewal in the present tense and none
  of it is implemented. It does **not** block the default path — self-signed
  certs are valid ten years — but it blocks ACME being trustworthy, which is
  what dev boxes and any domain-owning host will run. ~~Two things need
  deciding as part of re-scoping #541 onto doctor: the scheduler and the
  swap.~~ **Resolved (D6 decisions 2–4):** one-shot `doctor tls renew` on a
  platform scheduler; the swap is in-process hot reload
  (`ReloadableCertResolver`) triggered by a throttled mtime re-check.
  Implemented in the D6b PR, which also folds in #541's smaller calls:
  `acme.json`'s `$VAR` credential indirection becomes live (renew resolves
  it unattended), the fixed DNS-propagation sleep becomes configurable plus
  an order-level retry, `post_renewal_hooks` execute, and the ACME order
  flow gets its first end-to-end coverage (Pebble-backed BDD — see
  `docs/services/doctor.md` §Renewal and `docs/skills/testing.md`).
  ADR-002's renewal sections were rewritten to the as-built design with
  phase status markers, closing the documentation defect #541 opened with.
- **Credential rotation and recovery UX (D6) — resolved (D6 decisions 5
  and 7).** `doctor auth rotate` re-runs distribution; services pick up a
  rotated `server.auth` by restart (rotation is operator-initiated and rare —
  no auth reload machinery). Recovery for a forgotten credential is reading
  the canonical 0600 copy at `~/.config/rusty-photon/pki/credential`, which
  `--fix` also reuses when wiring a newly installed service.
- **How doctor detects what is installed (D2).** ~~Binary presence under a
  platform-specific prefix is the most portable; querying dpkg/rpm/SCM/brew is
  more accurate but is four implementations.~~ **Resolved (D2 decision 3):**
  ask the service manager for `rusty-photon-*` units, plus config-file
  presence for dev checkouts.
- **Whether `--fix` should refuse to run while services are live — resolved
  (D3): warn and proceed.** The canonical install flow (packages install,
  services auto-start, operator runs `--fix` once) has services running by
  design at exactly that moment, so refusing would break the primary
  workflow. Atomic renames make corruption impossible; the residual race (a
  driver's `config.apply` landing between doctor's read and write loses one
  of the two writes) is called out on stderr with the advice to re-run
  doctor and restart services to pick up fixed configs.
- **Which package ships doctor — resolved (D6 decision 8): sentinel's
  package.** ~~Its own `rusty-photon-doctor` package that the operator
  installs deliberately, or bundled into a common package.~~ Sentinel's
  deb/rpm/MSI/brew artifact carries the doctor binary and the renewal timer
  units; implemented in D7. Decision 1's operator-run model is unchanged — only
  the delivery vehicle is bundled.

## Future considerations

- **rp's ordinal device binding is unsound, and it is out of scope here.**
  `services/rp/src/equipment/camera.rs:81` binds a roster entry to a physical
  device by *counting* devices of the matching type until it reaches
  `config.device_number` — a `Vec` index over SDK bus order. Unplug the guide
  cam and reboot, and rp connects to the main imaging camera as `"guide-cam"`,
  reports `connected: true`, and dithers against it. Nothing detects it; the
  only failure implemented fires when the slot is *empty*, never when it is
  occupied by the wrong device.

  Every enumerated device already has a stable, hardware-derived UniqueID
  (`ZWO:{name}:{serial}`, the QHY SDK id), the driver already publishes it, and
  the client library already exposes it as `Device::unique_id()` — rp just
  never reads it. Adding `unique_id: Option<String>` to the roster and matching
  on it at connect time turns a silent 2am misbind into a startup error.

  This is device usage, so it belongs to rp and not to doctor, per Decision 4.
  It is recorded here because this analysis surfaced it and it bears directly
  on tenets #1 and #2. It wants its own issue.

- **Sorting enumerated devices by serial before registration** in zwo-camera
  and qhy-camera would make `device_number` stable under replug for the
  serial-bearing majority — a couple of lines in each `build()`, independent of
  everything above.

- **Alpaca UDP discovery** stays off, per
  `crates/rusty-photon-driver/src/discovery.rs:12`. Should a single-responder-
  per-host design ever be revisited, doctor's catalog is the natural source for
  what such a responder would advertise.

## References

- [ADR-016](../../decisions/016-service-config-ownership-and-doctor.md) — the
  decision record behind this plan: config ownership, the installer/doctor
  split, and the SDK line, with the rejected alternatives (runtime discovery,
  postinst generation, a joint file, a hardware-probing central doctor).
- [ADR-012](../../decisions/012-service-packaging-architecture.md) — config is
  user-based XDG, owned by the service; conffiles rejected because services
  rewrite their own config. Generation is not shipping: this plan does not
  reopen that decision.
- [ADR-013](../../decisions/013-native-sdk-payload-policy.md) — native SDK payload
  policy; proprietary firmware is never packaged.
- [ADR-014](../../decisions/014-zwo-per-device-services-and-link-features.md) —
  one service per independently usable device; each package ships exactly its
  own blob. Draws the SDK line this plan follows.
- [ADR-015](../../decisions/015-windows-packaging-architecture.md) — config and
  state are platform-dependent defaults in code, not installer artifacts.
- [config-actions](../../services/config-actions.md) — the `config.get`/`apply`/
  `schema` protocol doctor must not fight with.
- [packaging.md](../../packaging.md) / [packaging-windows.md](../../packaging-windows.md)
  / [packaging-macos.md](../../packaging-macos.md) — current per-platform reality.

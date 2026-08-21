# ADR-016: Service config ownership — installers place bytes, a standalone doctor wires them

## Status

Accepted (2026-07-15); amended 2026-07-16 (see [Amendments](#amendments-2026-07-16));
implemented in full and archived —
[`docs/plans/archive/service-config-doctor.md`](../plans/archive/service-config-doctor.md).

Builds on [ADR-012](012-service-packaging-architecture.md) §3 without
superseding it: packages still ship no config file, and services still
self-create their own on first start. This ADR settles what was left open —
who reconciles the facts that span services once those files exist.

Draws the central/per-service boundary from
[ADR-014](014-zwo-per-device-services-and-link-features.md)'s link policy;
that boundary is a consequence of ADR-014, not an independent choice.

## Context

ADR-012 made each service the sole owner of its own config file. That was
right, and it left a gap: **nothing owns the facts that span services.**

A rig running one ZWO camera, one EAF, a PPBA, and a mount declares that
camera **three times** — `rp`'s `equipment.cameras[].alpaca_url`, ui-htmx's
`drivers.{id}.base_url`, and sentinel's `services.{name}.base_url` — in three
vocabularies, with three URL conventions (sentinel wants an `/api/v1` suffix;
the other two do not) and two structurally-identical credential types
(`rp_auth::ClientAuthConfig` and ui-htmx's `DriverAuth`). One service name is
spelled four ways: the ui-htmx `drivers` key, its `sentinel_service` field,
sentinel's `services` key, and `operation_watchdog.operations.<family>.service`.
Nothing validates any of it; a mismatch surfaces at 2am as a 404 in a UI
banner. For that four-device rig the same plaintext password is typed nine
times across two files.

The rot is measurable. `ServerConfig` has **13 independent definitions**, seven
field-for-field identical, and five services (qhy-camera, zwo-camera,
zwo-focuser, sky-survey-camera, ui-htmx) carry no `tls`/`auth` at all and
therefore cannot be secured while their siblings can. `rusty-photon-<svc>` is
independently re-encoded in `.service` files, `.wxs` fragments,
`generate-brew-formulas.sh`, and `rig.sh` — and the copies drifted far enough
that **every documented `restart_command` in the repo is wrong twice over**
(`--user` scope against system units; missing the `rusty-photon-` prefix), one
still naming `qhyccd-alpaca`, a dead predecessor project, pointed at
filemonitor's port. Sentinel's unit runs `User=rusty-photon` with
`NoNewPrivileges=yes` and there is no polkit rule or sudoers fragment in
`packaging/`, so the field is decorative on Linux regardless of its contents.

### Alternatives considered and rejected

**Sentinel discovering restart facts by asking each driver** — rejected
because of a **liveness paradox**: sentinel's job is restarting things that are
dead, so any fact it learns by asking a driver is missing exactly when it is
needed. A cold start against an already-dead driver has no answer. The fix
(decision 8) is to ask a component that is *always* alive — the platform's
service manager, which is not a rusty-photon process, is up whenever the
machine is, and is the authority on what is installed and how to restart it.

The distinguishing test is not "static vs. dynamic" but **whether a component
must keep functioning while its source of truth is down.** Sentinel must — that
is the paradox. **ui-htmx must not**, which is why decision 9 lets it source
its driver list from rp even though rp can be down. When rp will not start, the
operator does not reach for the browser; they `ssh` in and run `doctor` (see
the recovery model under Consequences). A UI that goes blind during an rp
outage costs nothing, because it was never the repair path — so rp being a
sometimes-down source is harmless for ui-htmx in a way it is not for sentinel.

**Config generation inside postinst.** Rejected because package installs are
incremental and unordered, so generation would have to converge across N
postinsts running in any sequence. That is the wart the MSI's seed-once already
has — `docs/packaging-windows.md` tells operators *"after adding features to an
existing install, add the new service's entry by hand."* It would also invert
the dependency graph, putting a doctor package underneath all 17 services.

**A single joint config file.** It would kill the duplication by construction,
but every service with `config.apply` writes its config back; concurrent atomic
renames against one shared file means lost updates. The per-file model is what
makes those self-rewrites safe (ADR-012 §3).

**Conffiles** remain rejected for ADR-012's original reason (services rewrite
their own config). Note that ADR-012's argument never ruled out *generation* —
only *shipping*. A generated file is not a tracked conffile: no upgrade
prompts, and the service can still rewrite it. This ADR declines generation
anyway, on the ordering grounds above, not the conffile grounds.

**A single doctor that probes hardware directly.** Rejected by ADR-014. Such a
binary must link `libASICamera2` + `libEAFFocuser` + `libqhyccd` + every future
vendor blob, and therefore ship them all at the shared `/usr/lib/rusty-photon/`
path — recreating the exact dpkg "two owners of one file" conflict ADR-014 was
written to fix, and forcing a QHY-only rig to install ZWO's SDK. This is a hard
blocker, not a size objection.

## Decision

1. **Installers place bytes on disk. A standalone `rusty-photon-doctor` wires
   the configs.** Packages do not generate or seed config and postinst does not
   call doctor. Services self-create their defaults as they already do
   (ADR-012 §3, unchanged), and the operator runs `rusty-photon-doctor --fix`
   to make the install coherent. An operator-run doctor sees the whole system
   at once and converges in one pass — no ordering problem, no idempotent
   merge, no dependency inversion. The cost is an explicit post-install step,
   accepted: it is the same shape as `postgresql-setup initdb`.

2. **One config file per service. Not a joint file.** The correctness burden
   moves onto doctor rather than onto a locking scheme.

3. **Doctor is a standalone binary, not a component of the services.** It links
   no service crate. It knows the catalog, the two shared `ServerConfig`
   shapes (core and Alpaca — see Amendments), and ui-htmx's `drivers` map;
   every other byte of every config file is opaque `serde_json::Value` it
   steps around.

4. **Doctor's scope is service facts, never device usage.** "Is `/dev/ttyUSB0`
   writable", "do two services claim one port", "does this `sentinel_service`
   name resolve" are in scope. Which camera is the guide cam, dark-library
   setpoints, focal length, and device identity binding are **usage**, owned by
   `rp`. Doctor never needs to know a serial exists.

5. **Hardware checks split at the SDK line.** Everything needing no vendor blob
   — device node presence, writability, group membership, udev rule
   installed *and parsed*, VID:PID in sysfs, firmware helper run — belongs to
   central doctor. Everything needing a blob belongs to a `doctor` subcommand
   on the service binary that already links it. Central doctor aggregates over
   two naturally exclusive paths: when a service is **up** it already
   enumerated its hardware, so ask `/management/v1/configureddevices` over
   HTTP; when it is **down**, shell out to its `doctor` subcommand. Neither
   path needs an SDK in doctor, and neither contends for a device lock — which
   also enforces the rule that **doctor must never open hardware a running
   service holds**.

6. **The similarity lives in a shared library, not a shared binary.** Serial
   and USB reachability checks are near-identical across services, so they go
   in a crate that per-service doctors call. Adding service #201 means
   implementing a small trait, not editing a central binary. Centralizing the
   binary is what fails to scale; centralizing the library is what makes it
   scale.

7. **The doctor report schema parses permissively** — `#[serde(default)]`,
   tolerate unknown fields — the inverse of the `deny_unknown_fields`
   convention every config uses. The asymmetry is deliberate: a config typo
   must be fatal at startup, but a doctor and a service from different nightly
   builds must degrade to a partial report rather than refuse to run.

8. **Sentinel discovers its supervised services; the `services` map is
   deleted.** Not doctor-generated — *deleted*. A map doctor writes would go
   stale the moment a package is installed and stay stale until someone
   re-runs `--fix`; sentinel asking the platform costs nothing and is never
   stale. Sentinel already shells out to `systemctl`, so it is **same-host-bound
   by definition** and can only supervise units on its own machine. Every fact
   in that map follows from that:

   - **What exists** — enumerate `rusty-photon-*` from the service manager
     (`systemctl list-units` / `Get-Service` / `brew services list`). This is
     the authority, and unlike a driver it is alive when the driver is dead.
   - **`restart_command` / `health_command`** — derived from the unit name.
   - **`base_url`** — read the service's own `<svc>.json` for `server.port`.
     After decision 3's shared `ServerConfig` shapes, that is typed across
     all 18. The port then has exactly one home: the service that listens on
     it. This is a same-host file read, not a discovery protocol between
     running processes.
   - **Health probe URL** — derived from the port plus the service's class, a
     static catalog fact: Alpaca drivers answer
     `/management/v1/configureddevices` (no device number needed, so no device
     knowledge leaks in); the non-Alpaca services (rp, plate-solver,
     session-runner, calibrator-flats, phd2-guider, ui-htmx) answer `/health`.

   **Policy becomes constants, not fields:** `max_restart_duration` 300s;
   health poll 30s; failure threshold 3; restart backoff 60s doubling to a
   900s ceiling. Every value except the restart budget is the shipped default
   promoted to a constant, so behaviour is unchanged; 300s replaces the
   current 60s.

   **Health supervision becomes universal.** Presence of a `health` block is
   currently the opt-in; with the block gone, supervision is on for every
   discovered service. That is the tenet-#2 answer, and it removes a footgun
   where forgetting a block silently means no supervision.

   **"Not restartable" is not a thing.** `restart_command: null` is removed.
   Every rusty-photon service must come back when sentinel says so; a service
   that cannot is a bug to fix, not a configuration to record. The escape
   hatch's stated purpose — a remote MCU we cannot `systemctl` — is moot
   under same-host discovery, because such a device was never a local unit and
   is therefore never enumerated.

   **The credential is the one thing sentinel does not discover.** With auth on
   (decision 10), sentinel's health/abort probes against the drivers need the
   observatory credential, and no amount of host inspection yields a plaintext
   password. That is not a contradiction: sentinel *discovers* what the host
   inherently knows (units, ports, commands) and *receives* from doctor the one
   thing the host does not — the secret doctor minted (decision 10(e)).
   Discovery and minting are different sources for different facts, so sentinel
   keeps a doctor-written credential even though its `services` map is gone.

9. **ui-htmx's source of truth is rp's roster; its `drivers` map is an optional
   override, empty by default.** ui-htmx already derives driver targets from
   rp's equipment roster at runtime; the target shape keeps only that. Its own
   config shrinks to its listening port and where rp is (which defaults to
   `localhost` on the single-box deployment) — no second copy of the driver
   list anywhere.

   The reason a static map seemed necessary was a **liveness paradox**: if
   ui-htmx sources its list from rp and rp is down, the UI goes blind. That
   objection dissolves once the recovery model is explicit (see Consequences):
   when rp will not start, you do not reach for the browser — you `ssh` in and
   run `doctor --fix`. A blind UI during an rp outage costs nothing, because
   the UI was never the repair path for a down rp. So there is nothing for a
   duplicate config to survive *for*.

   The three forces that used to require the map have each dissolved
   independently: **auth redaction** (rp redacts per-device passwords, so
   roster-derived clients could not authenticate) is handled by decision 10(e)
   — doctor writes ui-htmx's credential directly, it does not depend on rp
   handing back a secret; **devices rp could not model** (the PPBA) are added
   to the roster by #534; and **restart wiring** (`sentinel_service`) is gone
   with decision 8. What remains of `drivers` is an escape hatch — a
   third-party device rp does not manage, or a driver deliberately given a
   separate credential — empty for a stock rig.

10. **Doctor owns the TLS and credential lifecycle; certs and credentials are
    generated by default; TLS and auth are both on.** The shared `ServerConfig`
    shapes carry `tls` and `auth` for **every** service — all 18, same knobs —
    and doctor turns both on in the config it writes. Simple and uniform
    is the whole point: a home observatory gets encrypted, authenticated
    inter-service traffic out of the box, with one CA and one credential, and
    the operator does nothing. Five parts:

    **(a) Doctor generates certs, by default, on `--fix`.** Self-signed, and
    the objections that apply to ACME do not apply here: `CA_VALIDITY_DAYS`
    and `SERVICE_VALIDITY_DAYS` are both **3650 (ten years)**, so there is no
    renewal problem; generation needs no domain, no email, and no API token, so
    nothing prompts; and unlike the config maps, certs carry **no
    cross-references** — each service's cert is independent and the CA is
    already create-if-absent, so there is nothing to converge.

    Doctor rather than postinst, on one remaining ground: doctor knows the
    derived catalog (decision 11). `rp_tls::cert::DEFAULT_SERVICES` today lists
    **five of eighteen** — a hand-typed list that rotted exactly like the other
    encodings of the service list, to the point that four services which *do*
    have `tls` fields (dsd-fp2, pa-falcon-rotator, pa-scops-oag,
    star-adventurer-gti) get no cert from the default command. Deriving it from
    the catalog fixes that by construction. Postinst is otherwise defensible
    here; it simply has no catalog.

    **(b) One location.** Certs move from `~/.rusty-photon/pki` to
    **`~/.config/rusty-photon/pki`**, and `acme.json` alongside the configs.
    Today certs and config live in two different hidden trees, and the
    `/etc/rusty-photon` discoverability symlink covers only one of them. One
    tree, one symlink, one thing to back up. Breaking for existing
    installations; sanctioned pre-1.0.

    **(c) The TLS commands move from `rp` to doctor** — issuance, ACME, and
    renewal. `rp init-tls` is removed. Cert provisioning is service-config
    work, which is doctor's remit (decision 4), and rp was only ever an
    arbitrary host for it: nothing about rp makes it the right process to mint
    zwo-camera's certificate. This also **splits `rp-tls`**: the serving half
    (`server`, `client`, `config`) stays a dependency of all 18 services, while
    the provisioning half (`cert`, `acme`, `acme_config`, `dns`) becomes
    doctor's alone.

    That split has a payoff beyond tidiness. `rp-tls` currently pulls
    `cloudflare` and `instant-acme` into **every service that merely wants to
    serve HTTPS**, and `install_default_crypto_provider` exists precisely
    because of it — *"both `aws-lc-rs` and `ring` end up feature-activated on
    rustls via our transitive deps (reqwest 0.13 + reqwest 0.12 via cloudflare
    rustls-tls)"*. Quarantining `cloudflare` to the doctor binary shrinks
    [#229](https://github.com/rusty-photon/rusty-photon/issues/229)'s blast
    radius from the whole workspace to one binary, and may retire the
    crypto-provider workaround for services entirely.

    **(d) On by default means doctor's generated config enables it**, not that
    the serde defaults become `Some(tls)` / `Some(auth)`. The distinction is
    load-bearing for both: packages start services at install
    (`WantedBy=multi-user.target`, WiX `Start="install"`) which is *before* any
    doctor run, so a serde default of "on" would leave every fresh install
    unable to start for want of certs and credentials that do not exist yet. It
    would also break every BDD and ConformU test that hand-writes a config
    omitting `tls`/`auth`. So: **absent `tls`/`auth` still means plain,
    unauthenticated HTTP**; doctor writes both on for every service it wires.
    Any real deployment runs doctor, so any real deployment is TLS + auth — one
    config, one doctor run.

    **(e) Doctor mints one observatory credential and distributes it.** Because
    doctor *generates* the credential, it holds the plaintext at generation
    time — so it writes the Argon2id **hash** into each service's `server.auth`
    and the **plaintext** into each client's auth block (rp's equipment
    entries, sentinel's supervised-service probes, ui-htmx's rp/driver
    targets), each in the form that side needs. This is what makes auth-on
    tractable rather than theatre: the objections to required auth were all
    "hand-maintaining N password copies is painful" — true, and exactly why a
    *machine* mints and distributes them. One credential, machine-written
    everywhere, zero hand-maintained copies. Password rotation is
    `doctor auth rotate` re-running the same distribution. This is
    [ADR-003](003-authentication-for-device-access.md)'s "Basic Auth **over
    TLS**" scheme, coherent for the first time because part (a)'s TLS sits
    underneath it.

    **The client-trust cost is accepted for now.** Self-signed means NINA,
    SGPro, and ConformU reject the handshake until the CA is installed in the
    OS trust store — the cost ADR-002's ACME path exists to avoid (`tls_cmd.rs`
    prints *"No CA configuration needed for clients — Let's Encrypt is publicly
    trusted"* on that path). In-stack clients are unaffected: they already take
    a `ca_cert_path`. Development and any host with a domain run
    `doctor tls --acme` and get publicly-trusted certs with no CA distribution
    at all.

    This **supersedes [#524](https://github.com/rusty-photon/rusty-photon/issues/524)**
    on mechanism while agreeing with its goal. #524's premise was wrong —
    zwo-camera, qhy-camera, zwo-focuser, and sky-survey-camera have no `tls`
    field to re-default — and its provisioning belonged in postinst rather than
    doctor.

11. **The catalog is derived, not typed.** `services/<svc>/pkg` existing is
   already the packaging authority — `build-packages.sh` and
   `generate-brew-formulas.sh` derive their service lists from it with a
   byte-identical line. Doctor's catalog comes from the same place, with a CI
   test asserting the table matches the tree, so it does not become the fifth
   independent encoding of `rusty-photon-<svc>` and rot like the other four.

## Consequences

- **The recovery model: config-actions tunes what runs; doctor repairs what
  won't start.** ui-htmx edits config by calling each service's own
  `config.get`/`config.apply` (drivers) or `PUT /api/config` (rp) — endpoints
  served *by that service's own HTTP server*. So a service broken badly enough
  to fail startup (rp exits from `load_config` before it binds; a driver never
  registers) is **uneditable over HTTP** — the editor is served by the thing
  that is down, and ui-htmx shows only a transport banner and Retry, no edit
  form. The repair path is therefore on-disk and out-of-process: `ssh` into the
  box and run `doctor --fix` (or hand-edit the file). This is not a gap — it is
  *why* doctor is an on-disk tool rather than another HTTP surface. Two facts
  keep it from being a burden: on the single-box deployment doctor sits on the
  same machine the operator already reaches by browser or ssh, so "same-host"
  is not "on the pier"; and the lockout is narrow — only configs that fail to
  parse or bind brick the HTTP path, while a merely wrong-but-valid value
  (a bad port, an out-of-range gain that still deserializes) leaves the service
  up and fully editable in the browser. Common mistakes stay in the UI; only
  genuine won't-start breakage needs doctor.
- A fresh multi-service install is **not coherent until someone runs doctor**.
  This is a real regression in "it just works" and is accepted in exchange for
  deleting the whole class of ordering bugs. It must be prominent in the
  install docs, not a footnote.
- The 13 `ServerConfig` definitions collapse into one shared crate
  (`rusty-photon-server-config`, two shapes), and the five services with
  ad-hoc listener configs convert in the same pass. That is a prerequisite,
  not a cleanup: it is what lets doctor parse the `server` block out of any
  `<svc>.json` while treating the rest as opaque, and therefore what keeps
  doctor out of the services. A breaking config-schema change
  (ui-htmx's `bind` → `bind_address`) rides along, sanctioned pre-1.0.
- **Nine services gain TLS and auth** because they inherit the shared shapes.
- Doctor becomes a **third writer** of config files, alongside operator
  hand-edits and drivers' own `config.apply`. It must reuse
  `rusty_photon_config::save`'s atomic temp→fsync→rename→fsync-dir path and the
  layer-aware persist rules, so it cannot bake a transient CLI override into a
  file. Atomic rename bounds the damage to a lost update, never corruption.
- **Sentinel cannot restart anything on a packaged Linux host today**
  (`NoNewPrivileges=yes` against system units, no polkit rule in `packaging/`),
  and decision 8 makes that blocking rather than cosmetic: once "not
  restartable" is removed and supervision is universal, *every* discovered
  service depends on that privilege path. It is tracked as
  [#523](https://github.com/rusty-photon/rusty-photon/issues/523), which already
  carries a scoped polkit rule verified on the rig, gated on
  `unit.indexOf("rusty-photon-") == 0 && verb == "restart"` — the same set
  decision 8 enumerates. Shipping it is a **prerequisite** for the sentinel
  work. Note #523 describes the rule as inert until sentinel's `services` map
  is populated; decision 8 deletes that map, so the rule is live for every
  discovered service. The Windows analogue (service account vs
  `Restart-Service`) is open.
- **[#524](https://github.com/rusty-photon/rusty-photon/issues/524) is superseded
  on mechanism but vindicated on goal** — decision 10 arrives at TLS-on *and*
  auth-on, by a different route (doctor, not postinst) and on a corrected
  premise (four Alpaca drivers had no `tls` field to re-default). Both its
  transport half and its "separable" auth half are adopted; close it.
- **`rp` loses `init-tls` and `hash-password`**; both move to doctor, along
  with `rp`'s `acme_setup.feature` / `tls_setup.feature` and `bdd-infra`'s
  one-shot command tests. `hash-password` follows because decision 10(e) makes
  doctor the credential minter — hashing is part of that lifecycle, and rp was
  only ever an arbitrary host for the command. A breaking CLI change,
  sanctioned pre-1.0.
- **Renewal needs a scheduler, not a background task.** ADR-002 specifies
  *"a background tokio task in `rp serve`"*; with the commands in doctor —
  a one-shot tool — renewal becomes `doctor tls renew` driven by a systemd
  timer, a Windows scheduled task, or a launchd interval. That is the
  conventional shape (it is what certbot does) and it is strictly better than
  the ADR's: it does not require rp to be running to renew zwo-camera's
  certificate. [#541](https://github.com/rusty-photon/rusty-photon/issues/541)
  needs re-scoping accordingly. Whether the post-renewal swap is a
  `ReloadableCertResolver` or simply a restart via sentinel (decision 8 makes
  every service restartable) is **open** — restarting is far simpler, but
  must not happen mid-exposure.
- **Sentinel's restart endpoint is protected by the same default.** Once
  #523's polkit rule ships and supervision is universal, that endpoint becomes
  a working, LAN-reachable control that can restart the entire observatory —
  so it must not be open. Decision 10 puts both TLS and auth in front of it by
  default: encrypted transport plus the doctor-minted credential. This is
  ADR-003's "Basic Auth over TLS" working end to end, and it is why auth-on is
  the target rather than a deferred option — a control this powerful should not
  ship unauthenticated even on a trusted LAN.
- **This breaks already-merged code.** The per-service `health` block shipped
  in #505 (merged 2026-07-13); decision 8 deletes it along with the whole
  `services` map. A breaking config-schema change, sanctioned pre-1.0. The
  design choices settled in that PR survive the move — never-give-up backoff
  becomes the constant-driven default, and no-recovery-notification is
  untouched.
- **Sentinel gains a dependency on the shared `ServerConfig` shape**, because
  it reads other services' `<svc>.json` for their ports. That is a real
  coupling, accepted because sentinel is already same-host-bound and because
  the alternative is a copy of every port in sentinel's own config. It also
  makes decision 3's shared type a prerequisite for the sentinel work, not just
  for doctor.
- **Health supervision becomes universal**, so a service that flaps now gets
  restarted where previously a missing `health` block silently meant no
  supervision. This is the intended robustness gain, but it means the first
  deployment will surface flapping that was previously invisible.
- Each hardware-touching service grows a `doctor` subcommand. The shared crate
  keeps that to a handful of calls per service.
- The report schema is a contract between two independently-upgradable
  binaries, so it needs versioning discipline that configs do not.
- ADR-012 §3, ADR-013, and ADR-014 are all unchanged; this ADR is additive to
  each.

## Amendments (2026-07-16)

Each modifies the decision text above in place; 1–4 were settled while
starting D1.

1. **Two `ServerConfig` shapes, not one** (modifies decisions 3, 8, 10). The
   shared crate carries `ServerConfig` (`port`, `bind_address`, `tls`, `auth`)
   for non-Alpaca services and `AlpacaServerConfig` (the same plus
   `discovery_port`) for the 11 Alpaca drivers. One shape would make
   `discovery_port` an accepted-but-inert knob on rp and ui-htmx — the silent
   footgun `deny_unknown_fields` exists to prevent. The Alpaca shape declares
   all five fields explicitly (serde's `deny_unknown_fields` does not compose
   with `flatten`); a common-subset accessor keeps one view for doctor and
   sentinel.

2. **The crate is `rusty-photon-server-config`** — workspace-infra naming,
   consistent with `rusty-photon-config` / `rusty-photon-shared-transport`.
   It depends on `rp-tls` and `rp-auth` for the embedded types and has no
   dependency in either direction with `rusty-photon-config`, which stays
   `serde_json::Value`-based.

3. **`bind_address` gets a unified default of `0.0.0.0`** across all 18
   services. This is D1's one deliberate behaviour change: six services (rp,
   ui-htmx, plate-solver, session-runner, calibrator-flats, phd2-guider)
   previously defaulted to `127.0.0.1`. Existing installs with explicit
   values in their files are unaffected; ones relying on the old defaults
   pick up the new one, and where the schema changed shape (`port` required
   when the block is present; ui-htmx's `bind` rename) they fail loudly at
   next start and need a one-line edit. The interim exposure is accepted
   because decision 10 makes TLS + auth the default for every doctor-wired
   deployment.

4. **D1 covers all 18 services in one pass** — the 13 existing `ServerConfig`
   definitions plus the five ad-hoc listener configs (sentinel, plate-solver,
   session-runner, calibrator-flats, phd2-guider). Nine services gain
   `tls`/`auth` fields (sentinel's dashboard already has both and converts
   shape-only), every service ends with a top-level `server` block, making
   decision 10's "every service has the knobs" premise true immediately
   after D1.

5. **The sentinel privilege path shipped**
   ([#523](https://github.com/rusty-photon/rusty-photon/issues/523) resolved;
   updates the Consequences bullet on sentinel restarts). The sentinel
   deb/rpm ships the rig-verified scoped polkit rule at
   `/usr/share/polkit-1/rules.d/50-rusty-photon-sentinel.rules` — the
   `restart` verb on `rusty-photon-*` units, `rusty-photon` user only — so
   decision 8's prerequisite is met on packaged Linux, and the grant never
   depended on sentinel's `services` map, so deleting that map changes
   nothing about it. The Windows analogue is moot: the MSI installs every
   service, sentinel included, under `LocalSystem`, which may restart
   services. macOS `brew services` run as the operator's own user and cross
   no privilege boundary.

6. **The `drivers` override map is deleted entirely** (2026-07-18; modifies
   decision 9 —
   [#569](https://github.com/rusty-photon/rusty-photon/issues/569), settled by
   field review of the rig). Decision 9 kept the map as an empty-by-default
   escape hatch for "a third-party device rp does not manage, or a driver
   given a separate credential". Neither survives: **there are no devices
   useful in this app's context that are not known to rp** — every device
   belongs in rp's equipment roster (this is a statement about *devices*;
   the driver *processes* stay supervised by sentinel, and the device/driver
   separation stands exactly as planned) — and the separate-credential
   rationale is superseded by decision 10(e) (doctor mints ui-htmx's
   credential). So ui-htmx's config-page targets shrink to two kinds (`rp`
   itself and roster-derived `rp:{kind}:{id}`), the `rp` target becomes
   **required** (an rp-less BFF has no purpose; a config without the block
   fails loudly), and a config still carrying `drivers` fails loudly at load
   with the deletion in doctor's `config.retired-keys` fix catalog — the
   sentinel `services`-map precedent. The restart affordance the map's keys
   used to wire is **derived instead of configured**: sentinel's
   `GET /api/services` exposes each discovered service's `probe_port`, and
   the BFF matches a roster device's `alpaca_url` port (same-host guarded)
   to find the service its restart button names.

7. **Three `ServerConfig` shapes, and no service may define its own**
   (2026-08-01; modifies amendment 1 —
   [#812](https://github.com/rusty-photon/rusty-photon/issues/812), found on
   the rig). The shared crate gains `AdvertisingServerConfig` (the core
   shape plus `advertised_url`) for services that advertise their own URL
   to another process — rp alone today, handing an orchestrator its MCP
   endpoint. Amendment 1's reasoning is why this is a third shape rather
   than a field on the core one: `advertised_url` on ui-htmx or sentinel
   would be exactly the accepted-but-inert knob that argument rejects.

   The stronger half of the amendment is the prohibition. rp had been
   carrying its own copy of the core shape plus the extra field, and the
   copy is what caused the bug: doctor validated rp against the shared
   `ServerConfig`, so it hard-`fail`ed a config rp accepts and started
   fine on — and, because an unparseable block makes every TLS/auth check
   self-limit, it silently stopped diagnosing TLS and auth for **rp
   specifically**, the one service that talks to everything. The failure
   presented as one complaint about a field name, not as eight missing
   checks, and survived long enough on the rig to be misfiled as a stale
   binary. A service-local shape is therefore not a local choice: it
   makes doctor wrong about that service. Every `server` block is one of
   the three shared types, declared by `class` in `pkg/doctor.toml`, and
   a service needing a field none of them carries adds a fourth shape
   here.

   Independently, doctor now reports what a shape failure costs
   (`config.checks-skipped`) instead of letting the missing rows pass for
   a clean bill of health.

## References

- Plan (phases, verification matrix, flagged unknowns):
  [`docs/plans/archive/service-config-doctor.md`](../plans/archive/service-config-doctor.md)
- Config ownership this builds on: [ADR-012](012-service-packaging-architecture.md) §3
- Native SDK payload policy: [ADR-013](013-native-sdk-payload-policy.md)
- The link policy that draws the SDK line:
  [ADR-014](014-zwo-per-device-services-and-link-features.md)
- Config-and-state-in-code, not installer artifacts:
  [ADR-015](015-windows-packaging-architecture.md) §4
- The edit protocol doctor must not fight with:
  [`docs/services/config-actions.md`](../services/config-actions.md)
- Config machinery doctor reuses: `crates/rusty-photon-config`
  (`resolve_config_path`, atomic `save()`, layer-aware persist)
- Precedent for shared-mechanism / per-service-data:
  `packaging/postinst.udev-stanza` (shared) +
  `services/*/pkg/90-rusty-photon-*.rules` (per-service)
- Open issues this interacts with:
  [#523](https://github.com/rusty-photon/rusty-photon/issues/523) (sentinel's
  polkit rule — a prerequisite for decision 8) and
  [#524](https://github.com/rusty-photon/rusty-photon/issues/524) (TLS by
  default — **superseded** by decision 10, which adopts both its transport and
  its auth halves; close it)
- TLS mechanics decision 10 leans on, and the renewal gap it turns on:
  [ADR-002](002-tls-for-inter-service-communication.md). Phases 2 and 3 of that
  ADR (cert hot-reload, background renewal, `rp renew-tls`, Pebble tests) are
  **documented in the present tense but not implemented** — `crates/rp-tls`
  issues certificates and nothing renews them. Tracked as
  [#541](https://github.com/rusty-photon/rusty-photon/issues/541); landing it
  reopens decision 10's install-time-provisioning half.

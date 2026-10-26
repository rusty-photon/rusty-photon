# Config actions + the BFF web UI

**Status: COMPLETE (archived 2026-07-12).** All 5 phases landed. Phase 1
(dsd-fp2 `config.get`/`config.apply` + in-process reload) and Phase 2 (the BFF
skeleton + dsd-fp2 config page as the `ui-htmx` service,
[`docs/services/ui-htmx.md`](../../services/ui-htmx.md)) shipped via PR #305
(`65ecc2d`). Phase 3 (the `config.get`/`config.apply`/`config.schema` protocol
generalised across **all six** drivers via the shared `rusty-photon-config::actions`
module, [`docs/plans/archive/config-actions-protocol.md`](config-actions-protocol.md),
with the `ui-htmx` BFF rendering **any** driver's form generically from
`config.schema` under `/config/{service}`) shipped via PR #344 (`1da776b`).
Phase 4 (Sentinel's `POST /api/services/{name}/restart`, driven by the
top-level `services` registry, plus the BFF's "Restart via Sentinel" button
and `restart_required` escalation) shipped via PR #488 (`13280fb`); decisions
resolved 2026-07-10 — see [Decisions (2026-07-10)](#decisions-2026-07-10).
Phase 5 (rp config actions, the roster-derived `/equipment` page, and the
`/stream` activity feed over an SSE proxy) shipped via PR #485 (`8665c9f`) and
follow-up PR #487 (`736d768`). Key protocol decisions resolved 2026-05-24 —
see [Resolved](#resolved-2026-05-24).
**Companion to:** [`mocks/README.md`](../ui-design/mocks/README.md) (the chosen UI direction and stack).

## Summary

This plan covers the first concrete slice of the rusty-photon web UI: the
**configuration pages** that configure the Alpaca driver services and (later)
`rp` itself. It defines:

1. A **standalone BFF service** (server-rendered axum + Maud + HTMX) as the home
   of the web UI — a client of the rest of the system, per `rp.md` tenet 7.
2. A **config-action protocol** by which each Alpaca driver exposes its own
   configuration over HTTP, modelled as **ASCOM `Action`s** (`config.get`,
   `config.apply`), with an **in-process reload** to apply changes without a
   process bounce.
3. A **service-lifecycle split**: drivers reload *themselves*; **Sentinel** owns
   *process* restart (`service.restart`) via the already-designed configured-command
   supervisor. These are independent capabilities — config editing does not
   depend on Sentinel.

The first deliverable is a working configuration page for **`dsd-fp2`**
(single `CoverCalibrator`, smallest config), proving the pattern end-to-end
before it is generalised to the other drivers.

## Context and decisions

The activity-stream mocks (`mocks/`) settled the *look* and *stack* of the UI
but cover only the session/telemetry experience. This plan starts implementation
at the **Settings/Equipment** end instead, because configuration is the
least-blocked, most self-contained surface and is a prerequisite to running
anything. Decisions reached during design exploration:

| Decision | Choice | Why |
|----------|--------|-----|
| UI host | **Standalone BFF service** (axum + Maud + HTMX + SSE) | Keeps `rp` a "dumb pipe" (`rp.md` tenet 7); matches `mocks/README.md` and i18n Option A. |
| Driver config transport | **ASCOM `Action`s** (`config.get` / `config.apply`) | Host-agnostic HTTP; reuses the Alpaca server each driver already runs; precedent in `star-adventurer-gti` (`mount_device/device.rs`). |
| Applying config | **In-process reload**, not a process bounce | The `rusty-photon-service-lifecycle` crate already supports reload (`with_reload`/`run_with_reload`, used by `filemonitor`); needs no external supervisor and works in dev. |
| Process restart | **Sentinel** `service.restart(name)` via configured command | Already the sanctioned design (`rp.md:2759-2767`, watchdog plan's `Restarter`). Reserved for recovery/escalation. |
| First driver | **`dsd-fp2`** | Single device, smallest config, first shared-transport adopter. |
| Form rendering | **Hand-built first**, schema-driven later | Prove protocol + the designed look on one driver; generalise after. |
| No-`--config` driver | **Persist to the platform config directory** (e.g. `~/.config/rusty-photon/<service>.json` on Linux) | A config path is *always* resolvable, so editing is never disabled; startup/reload read it if present. |
| Driver auth on config actions | **No per-action gate**; rely on the server-wide auth/TLS the driver already runs | Matches ASCOM Alpaca — `action` is just another device method; auth and transport security are orthogonal concerns handled by `rp-auth`/`rp-tls`. |
| Config layers | **Distinguish file vs. CLI-override** | `config.get` marks CLI-override-pinned fields; `config.apply` does not persist them, so a transient `--port` can't be baked into the file. |

`config.set` was dropped in favour of a single `config.apply`: the set/apply
split only helped batch multiple writes, which does not arise when a form
submits the whole config blob at once.

## Architecture

```
                ┌───────────────────────────┐
   browser ───► │  BFF  (services/ui-htmx)   │  server-rendered HTML
                │  axum + Maud + HTMX        │
                └───┬───────────┬────────┬───┘
   config.get/apply │           │        │  service.restart(name)
   (ASCOM Action,    │   REST    │        │  (REST)
    PUT .../action)  ▼           ▼        ▼
              [dsd-fp2]   ...   [rp]   [Sentinel]
              [qhy-…]          (REST   (configured restart
              (Alpaca          config   command, e.g.
               devices)        later)   `systemctl restart …`)
```

**Transport asymmetry (important):** only the drivers are ASCOM devices, so only
they expose config via `Action`. `Sentinel` and `rp` are *not* devices — their
config / lifecycle endpoints are plain **REST** on their existing axum routers.
Same shapes, different transport. Do not try to make Sentinel or `rp` an Alpaca
device.

**Responsibility boundary:** *in-process state rebuild* belongs to the driver
(only it can rebuild its own server + transport); *process existence* belongs to
Sentinel/OS. Hence `config.apply` (driver, reload) and `service.restart`
(Sentinel, process) are different tools for different blast radii.

## The config-action protocol (drivers)

Each driver advertises and serves two vendor actions through the standard ASCOM
`Device` trait (`ascom-alpaca`'s `action` / `supported_actions`). Working names
(align the exact strings with the convention in `star-adventurer-gti`'s `actions`
module during implementation):

```
GET  /api/v1/{type}/{n}/supportedactions     → ["config.get","config.apply", …]

PUT  /api/v1/{type}/{n}/action
       Action=config.get      Parameters=
   → 200, body = {
       "config":    <current effective Config as JSON, secrets redacted>,
       "overrides": ["serial.port"]   // CLI-override-pinned; config.apply won't persist these
     }

PUT  /api/v1/{type}/{n}/action
       Action=config.apply    Parameters=<full Config JSON>
   → 200, body = {
       "status": "applying" | "ok" | "invalid",
       "applied":          ["cover_calibrator.max_brightness"],  // took effect live
       "reload":           ["serial.port","server.port"],        // applied via reload
       "restart_required": [],                                   // needs Sentinel.restart
       "skipped_override": ["serial.port"],                      // override-pinned, not persisted
       "persisted_to":     "~/.config/rusty-photon/dsd-fp2.json",
       "errors":           [ {"path":"serial.baud_rate","msg":"…"} ]  // when invalid
     }
```

For `dsd-fp2` the device is `covercalibrator/0` and the Config is:

```
Config { serial:{port, baud_rate, polling_interval, timeout},
         server:{port, discovery_port, tls, auth},
         cover_calibrator:{name, unique_id, description, enabled, max_brightness} }
```

### `config.apply` behaviour

1. Parse `Parameters` as the driver's typed `Config`. Parse failure → ASCOM error.
2. **Validate** (types + ranges + semantics). On failure return **HTTP 200** with
   `{"status":"invalid","errors":[…]}` — a *domain* error the BFF renders as
   field-level messages, distinct from a transport/ASCOM error.
3. **Persist** the new config atomically to the resolved config path
   (stage temp → fsync → rename → fsync dir), creating parent dirs for the
   platform default. **CLI-override-pinned fields are written through from the file's
   prior value, not the submitted value** (layer-aware persist), and listed in
   `skipped_override[]`.
4. **Classify** each changed field into `applied` (live), `reload`, or
   `restart_required`, and **fire the in-process reload** if anything is in
   `reload` — *after the response is flushed* (see below).
5. Return immediately with the classification. `status:"applying"` signals the
   BFF that a reload is in flight and it should reconnect and re-`config.get`
   to confirm.

### Rules

- **Works while disconnected.** Config actions must not require `Connected=true`,
  or a wrong `serial.port` becomes unfixable (can't connect to fix the thing that
  blocks connecting). This is a deliberate choice in our `action()` impl.
- **Always has a config path.** A persist target is *always* resolvable: the
  explicit `--config` path if given, else the platform config directory (e.g.
  `~/.config/rusty-photon/<service>.json` on Linux). Startup and reload read this path
  if it exists (falling back to `Config::default()` otherwise), and
  `config.apply` persists there. Editing is therefore never disabled for lack of
  a path. (Resolved — see Open questions; previously this rejected when started
  without `--config`.)
- **Layer-aware persist.** `config.get` reports which fields are pinned by CLI
  overrides (`--port`, `--server-port`); `config.apply` persists every field
  *except* those, so a transient override is never baked into the file. Skipped
  fields are echoed in `skipped_override[]`.
- **No per-action auth gate.** `config.get`/`config.apply` are ordinary ASCOM
  actions and are exactly as protected as `calibrator_on` — by whatever
  server-wide `rp-auth`/`rp-tls` the driver runs, not a special case. Auth and
  transport security are orthogonal concerns; the action layer does not know
  about them. (Resolved — see Security and Open questions.)

## In-process reload mechanics

Adopting reload on `dsd-fp2` means switching `main.rs` from `.run()` to
`.with_reload().run_with_reload(...)` and moving config-loading *inside* the loop,
mirroring `filemonitor`:

```rust
ServiceRunner::new("dsd-fp2")
    .with_reload()
    .run_with_reload(move |shutdown, reload| async move {
        loop {
            let config = load_effective_config(&config_path, &overrides)?; // re-read each cycle
            let bound = ServerBuilder::new().with_config(config).build().await?;
            // Stop on shutdown *or* reload, recording which fired, and await
            // start() to completion so the server's own teardown runs.
            let reloaded = Arc::new(AtomicBool::new(false));
            let stop = {
                let reloaded = Arc::clone(&reloaded);
                let shutdown = shutdown.cancelled();
                let reload = reload.clone();
                async move {
                    tokio::select! {
                        () = shutdown => {}
                        () = reload.recv() => reloaded.store(true, Ordering::SeqCst),
                    }
                }
            };
            bound.start(stop).await?;
            if reloaded.load(Ordering::SeqCst) { continue; } // rebuild from new config
            return Ok(());
        }
    })
```

Three mechanics must be handled deliberately:

1. **Programmatic reload trigger.** `ReloadSignal`
   (`crates/rusty-photon-service-lifecycle/src/reload.rs`) is `Clone` and already
   exposes a public `notify()`, documented as the hook for "non-signal-driven
   reload sources" beyond SIGHUP / SCM `ParamChange`. So the `config.apply` handler
   just holds a clone of the `ReloadSignal` in the axum app state and calls
   `.notify()` — **no lifecycle-crate change needed.** Caveat: the signal is
   single-consumer (`notify_one`), so exactly one task may `recv()` it (the run
   loop), which is already the intended pattern.
2. **Fire-after-response.** The `config.apply` request is served by the very
   axum server the reload tears down. If reload fires synchronously, the response
   never flushes. So the handler must **return the response, then fire reload**
   (e.g. `tokio::spawn` a task that yields/short-sleeps, then triggers). The BFF
   treats `status:"applying"` as "expect a brief connection blip; reconnect and
   re-`config.get`." This is the same race a process restart would have; reload
   still wins (no supervisor, no cross-process socket churn, faster).
3. **Clean teardown.** The shared transport prefers explicit `close().await` over
   drop-teardown ("`close().await` primary, `Drop` detached fallback"), so the old
   server must tear down before the rebuilt one binds. Rather than dropping the
   `start()` future on reload, the loop passes a combined *stop* future
   (shutdown-or-reload, recording which fired) into `bound.start(stop)` and
   **awaits it to completion**. `BoundServer`'s own shutdown then runs —
   gracefully draining HTTP connections and calling `transport.shutdown()` to
   release the serial port — before the loop rebuilds from the new config, so the
   port is never double-held. (Reconciled with #302's service-lifetime transport
   lifecycle, which made `start()` own the teardown.)

If a field cannot be reloaded cleanly, `config.apply` returns it in
`restart_required[]` and the BFF escalates to Sentinel — keeping the protocol
honest about (3).

## Service lifecycle (Sentinel)

`service.restart(name)` is **not** part of the normal config path; in-process
reload handles config-file changes. It is the recovery hammer (wedged/crashed
driver) and the escalation for `restart_required` fields. It reuses the
already-designed supervisor:

> *"Sentinel executes the configured restart command for that service (e.g.
> `systemctl restart qhyccd-alpaca`)… configured per service, not hardcoded."* —
> `rp.md:2759-2767`

Implementation: expose the planned `Restarter` trait (see the watchdog plan,
[`predictive-deadlines-and-watchdog.md`](predictive-deadlines-and-watchdog.md))
as a REST endpoint on Sentinel's
existing dashboard router (e.g. `POST /api/services/{name}/restart`), driven by a
per-service `restart_command` config entry. Sentinel does **not** spawn or own the
processes — it shells out to the configured command; the OS supervisor (systemd /
SCM) owns relaunch. This is a larger workstream tied to the watchdog plan and is
**not a prerequisite** for the config pages.

## The BFF service (`services/ui-htmx`)

The crate is **`ui-htmx`** — the first member of a `ui-*` family of UI
expressions: browser variants are qualified by technology (`ui-htmx`, future
`ui-leptos`), native ones by target (future `ui-ios`, `ui-android`), and
shared backend-for-frontend logic is extractable to `ui-core` when a second
expression lands. Per the crate-naming convention it is an unprefixed service
under `services/` (system-wide, not `rp`-specific). Full design:
[`docs/services/ui-htmx.md`](../../services/ui-htmx.md).

- **Stack:** axum + [Maud](https://maud.lambda.xyz/) + [HTMX](https://htmx.org/),
  assets embedded via `include_str!`. Dark theme reusing the mocks' CSS tokens so
  config pages are visually consistent with the eventual activity stream. No npm,
  no WASM. (SSE comes later with live telemetry; config pages need none.)
- **Phase-1 routes:**
  - `GET /config/{service}` → call the driver's `config.get`, render a hand-built
    Maud form filled with current values.
  - `POST /config/{service}` → build the Config JSON from the form, call the
    driver's `config.apply`. On `status:"invalid"`, re-render the form with
    field-level errors (HTMX swap). On `status:"applying"`, render an
    "applied — reconnecting…" state and poll `config.get` until the driver is back.
- **Driver addressing:** Phase 1 hard-codes one target (the `dsd-fp2` Alpaca URL +
  `covercalibrator/0` + optional auth) from BFF config. Later the device list is
  derived from `rp`'s `GET /api/equipment` or ASCOM discovery.
- **HTTP client:** follow sentinel's `HttpClient` trait + `mockall` pattern so
  handlers are unit-testable without a live driver.

## Security

- **No per-action auth gate (resolved).** `config.apply` is a write like
  `calibrator_on`; it is protected by the same server-wide `rp-auth`/`rp-tls`
  layer the driver already runs, *not* by a special check inside `action()`.
  Auth and transport security are orthogonal concerns, handled once at the
  server boundary — this matches ASCOM Alpaca, where `action` is just another
  device method. A driver exposed without auth has *every* write open, not just
  config; that is a deployment choice, not something the config action
  second-guesses.
- **Redact secrets in `config.get` (hygiene, not a gate)**: never emit the
  `auth` password/hash or TLS key material in the config dump, regardless of who
  is authorised to read it. On `config.apply`, treat absent/sentinel secret
  fields as "unchanged" so a round-tripped form does not blank them.
- The BFF holds driver credentials to call the actions; store them in the BFF's
  own config, not in the page.

## Testing strategy

Per `docs/skills/testing.md` (prefer `unwrap()`-style failures; smallest unit
that is still comprehensive in aggregate) and the BDD setup in `tests/`.

- **Driver unit tests** (run in `mock` mode):
  - `supported_actions` lists the config actions.
  - `config.get` returns current config; secrets redacted.
  - `config.apply` with valid JSON persists + classifies; with invalid JSON →
    `status:"invalid"` and the file is unchanged; unknown action →
    `ACTION_NOT_IMPLEMENTED`; works while disconnected; works without auth (no
    gate); CLI-override-pinned fields are reported in `overrides[]`/
    `skipped_override[]` and not persisted.
- **Driver reload test:** `config.apply` triggers reload and the server returns
  with the new config (BDD via `bdd-infra`, or an integration test against the
  bound server). Assert the fire-after-response ordering (the apply response is
  received before the blip).
- **BFF tests:** the `config_page.feature` BDD suite spawns the *real* BFF and a
  *real* `dsd-fp2` (mock mode) and drives it over HTTP end to end — render,
  apply → reload → reconnect (serving the new value back), unchanged → `ok`,
  validation, and unreachable-driver. A handler unit test covers the one error
  state a live driver can't produce (`ACTION_NOT_IMPLEMENTED`) via a stub
  `ConfigClient`; `driver_client` unit tests cover envelope parsing with a
  mocked `HttpClient`.

## Federated roster: managed (own) vs. foreign devices

The config-action protocol above governs **drivers we author**. The web UI will
eventually administer a mix of those and **foreign native-Alpaca devices** we do
*not* author (e.g. a Pegasus Falcon v2 on its own firmware, an Optec focuser, an
ASCOM Remote server, a second rusty-photon stack). These form two tiers,
distinguished by one axis — *can we reach a structured config surface?* — and the
roster records that tier per device.

- **Managed (own drivers).** We write the Alpaca server, so it advertises
  `config.get`/`config.apply` and renders in the native form: validation,
  redaction, override-aware persist, in-process reload.
- **Foreign (third-party firmware).** They will never speak `config.*`. We can
  still **control** them over the standard Device API — the `ascom-alpaca` crate
  we maintain already ships a client (`Client`, `get_devices()`, typed per-type
  device clients) — but we can only **configure** them by deep-linking their own
  `/setup` page or a vendor page (e.g. `falcon2.local`), or by surfacing
  `SupportedActions` as an opaque "advanced commands" panel.

**Capability detection is one probe, not a hardcoded table:** read
`supportedactions`; `config.*` present ⇒ *managed*; else a reachable
`/setup/v1/{type}/{n}/setup` ⇒ *setup-page*; else *control-only* (plus an optional
per-vendor override URL). Because `config.*` is already self-advertising via
`supportedactions`, any third party that adopts the convention auto-upgrades to
*managed*.

**Roster entry model** — keyed by the ASCOM `UniqueID` (the spec guarantees it
stable across IP changes), not by URL:

```rust
struct RosterEntry {
    unique_id: String,           // ASCOM UniqueID — stable PK
    name: String,
    device_type: DeviceType,
    device_number: u32,
    server: ServerRef,           // { base_url, auth, ca_cert } — per-server credentials
    mgmt: Management,            // Managed | SetupPage{url} | VendorPage{url} | ControlOnly
    origin: Origin,              // Manual (primary) | Discovered (pre-fill) | RpRoster
}
```

This generalises the BFF's current single hard-coded `dsd-fp2` `DriverTarget`
into a keyed collection sourced from `rp`'s equipment registry (the backend
already exists — `services/rp/src/equipment/`, `GET /api/equipment`) **plus manual
entries**. The managed/foreign tier and the per-device control panels (driven by
the crate's typed clients) are the new parts; the registry and the client are not.

**Cardinality is many-per-type, except the mount.** An observatory runs one mount
but can run several of everything else — multiple cameras, focusers, rotators,
filter wheels — organised into one or more **optical trains** (each a camera +
focuser + rotator + filter wheel, all riding the single mount). The roster's flat
`UniqueID`-keyed collection already admits duplicates per type (`rp`'s registry
models this: `mount` is `Option`, the rest are `Vec`), so the control panels must
address each instance by `UniqueID` and never assume one-per-type. Optical-train
*grouping* is a rusty-photon overlay — **Alpaca has no optical-train concept**,
only a flat device list — so the pairing comes from `rp`'s config (the imaging
setup), not from discovery or the Management API.

See [Decisions (2026-05-27)](#decisions-2026-05-27) for the scope, cardinality,
credential, discovery, self-lockout, and convention-crate calls that shape this.

## Phased plan

**Phase 1 — `dsd-fp2` config actions + reload (driver-side, fully testable in mock mode). ✅ Landed.**
- Add the platform config-path resolver (`--config` else the platform config dir
  — e.g. `~/.config/rusty-photon/dsd-fp2.json` on Linux — via the `directories` workspace dep) +
  CLI-override tracking (`--port`, `--server-port`).
- Fire reload from `config.apply` via the already-public `ReloadSignal::notify()`
  (no lifecycle-crate change needed).
- Restructure `dsd-fp2` `main.rs` to `run_with_reload`; move config-loading into
  the loop; clean transport teardown on the reload arm via
  `device.set_connected(false).await` (no shared-transport change — `build()`
  keeps an `Arc<DsdFp2Device>`, `BoundServer::device()` exposes it).
- Implement `config.get` / `config.apply` actions (+ `supported_actions`),
  including validation, atomic persist, classification, layer-aware persist
  (skip CLI-override-pinned fields), fire-after-response reload, secret
  redaction. No auth gate.
- Tests as above. *Deliverable: a driver whose config you can read, write, and
  reload over HTTP.*

**Phase 2 — BFF skeleton + hand-built `dsd-fp2` config page. ✅ Landed.**
- New `services/ui-htmx` crate (axum + Maud + HTMX, embedded assets, dark theme).
- `GET/POST /config/dsd-fp2` wired to the driver's config actions; validation,
  override-pinned-field read-only, and "applying/reconnecting" states. 8 BDD
  scenarios that spawn the real BFF + a real mock-mode `dsd-fp2` and drive it
  over HTTP (including the full apply → reload → reconnect round trip), plus
  handler/wire-parsing unit tests. *Deliverable: a working config page for
  `dsd-fp2`.*

**Phase 3 — Generalise across drivers. ✅ Landed.** Both halves shipped in one PR,
along the natural driver ⇆ BFF fault line.

*Phase 3a — driver-side protocol. ✅ Landed.*
- ✅ Extracted the shared `config-actions` helper into the existing
  `rusty-photon-config` crate (`actions` module: the `ConfigurableDriver` trait +
  generic `config_get`/`config_apply`/`config_schema`). The cross-driver protocol
  is documented in [`docs/plans/archive/config-actions-protocol.md`](config-actions-protocol.md).
- ✅ Rolled out to **all six** drivers — `dsd-fp2` (refactored onto the shared
  crate), `qhy-focuser`, `pa-falcon-rotator`, `ppba-driver`, `sky-survey-camera`,
  `star-adventurer-gti` — each with a `config_actions.feature` BDD suite covering
  advertise / schema+tiers / get / apply→reload→rebind / invalid / unknown-action.
- ✅ Added `config.schema` (via `schemars`) as the **third action**: returns a
  JSON Schema plus the editability tiers (`locked_fields` / `read_only_fields`).
- ✅ The shipped Phase-2 `dsd-fp2` BFF page stays green unchanged — `config.get`
  keeps its `{config, overrides}` shape and `ApplyStatus { applying, ok, invalid }`
  is preserved verbatim, so `ui-htmx`'s existing parsing and `config_page.feature`
  are untouched.

*Phase 3b — schema-driven BFF renderer. ✅ Landed.*
- ✅ The `ui-htmx` BFF now renders **any** driver's form generically: `FieldModel`
  walks `config.schema` into a flat list of scalar leaves (resolving `$ref`,
  recursing plain objects, **skipping** `oneOf`/`anyOf`/`enum`/`const` subtrees,
  which round-trip via the hidden blob — keeping redacted secrets safe), and
  `merge_form` coerces submissions back per leaf (`string`/`bool`/`integer`/`number`,
  using the schema's `minimum`/`maximum`/nullability). The hand-built `dsd-fp2`
  field lists are gone.
- ✅ Editability tiers come straight from the driver: `config.get`'s `overrides[]`
  plus `config.schema`'s `locked_fields` / `read_only_fields`. No BFF-side
  per-driver lists remain, so a new driver needs no BFF change.
- ✅ Multi-driver routing: `AppState` holds a map of clients; `/config/{service}`
  (+ `…/status`) routes per service; the index lists every configured driver. The
  `config_page.feature` BDD gained a multi-driver scenario; `ui-htmx` reuses the
  shared `rusty_photon_config::actions` wire types (its duplicated copies deleted).
- ⬜ Follow-up (not blocking): a generic `oneOf`/enum (discriminated-union)
  renderer and a dedicated password input for redacted-secret leaves — until then
  those fields round-trip read-only via the blob and are edited in the config file.
- ✅ Landed (PR #344): the ASCOM-facing glue duplicated across the six drivers —
  the `config.apply` `ApplyError → ASCOMError` mapping, the `ConfigActionCtx` +
  generic action `dispatch`, and the common transport-driver error model — was
  extracted into the new **`rusty-photon-driver`** crate (the macro `driver_error!`
  generates each driver's error enum). `rusty-photon-config` stays the ascom-free
  protocol *model*; `rusty-photon-driver` is the ASCOM *adapter*. See
  [ADR-007](../../decisions/007-rusty-photon-driver-shared-crate.md).
- Future: promote the shared helper to a standalone vendor-neutral crate once it
  has settled (see [Decisions](#decisions-2026-05-27)).

**Phase 4 — Sentinel `service.restart`. ✅ Landed.**
- ✅ Sentinel exposes `POST /api/services/{name}/restart` on the dashboard
  router (behind the same `dashboard.auth`/TLS layers), reusing the watchdog
  ladder's already-implemented `Restarter`/`ShellRestarter`. The per-service
  registry is the **new top-level `services` map** (moved out of
  `operation_watchdog.services`, which is gone — the watchdog references the
  shared map), with `base_url` now optional so non-Alpaca services can be
  restart-only entries, a new optional shell `health_command` (exit 0 =
  healthy; polled after the restart to confirm recovery), and a per-service
  `max_restart_duration` (default 60s) replacing the watchdog-global knob.
  See [`docs/services/sentinel.md`](../../services/sentinel.md)
  §Service Restart API for the endpoint contract (200 ok/failed + recovery,
  404 unknown, 409 not-restartable / in-flight).
- ✅ The BFF renders a "Restart via Sentinel" button on every driver card when
  its config has a `sentinel` block (`{base_url, auth, ca_cert_path}`; each
  driver's Sentinel-side name comes from `sentinel_service`, defaulting to the
  service id), and escalates a non-empty `restart_required[]` from
  `config.apply` with an inline banner + the same button. A restart that
  Sentinel accepts flows into the existing reconnecting/poll fragment.
  See [`docs/services/ui-htmx.md`](../../services/ui-htmx.md)
  §Restart via Sentinel.

**Phase 5 — `rp` config + equipment roster + the activity stream. ⏳ In progress
(single PR).** Scope as built (details in
[`ui-htmx.md`](../../services/ui-htmx.md) and [`rp.md`](../../services/rp.md)):
- ✅ *(this PR)* `rp` gains `GET/PUT /api/config` + `GET /api/config/schema` —
  the same bodies as the driver actions, as plain REST, implemented on the
  shared `rusty_photon_config::actions` core (extended with **wildcard secret
  pointers** for the per-device auth arrays and an **`ApplyDisposition`** knob:
  rp has no in-process reload, so changes classify as `restart_required` /
  `status:"ok"` — honest until Phase 4 turns the callout into a Sentinel
  restart button).
- ✅ *(this PR)* The BFF gains the `rp` target: `/config/rp` reuses the whole
  schema-driven form machinery through a `RestConfigClient`; the **equipment
  page** (`/equipment`) joins rp's config (authoritative roster + addresses)
  with `GET /api/equipment` (live `{id, connected}`) and carries the
  managed/foreign **capability tier per device** via a live `supportedactions`
  probe (managed devices get roster-derived config pages at
  `/config/rp:{kind}:{id}` — no hand-added BFF entry needed). **Manual entry**
  (the primary path) is add/edit/remove of roster entries, schema-generated
  per kind, persisted by config surgery over `PUT /api/config`.
- ✅ *(this PR)* The **activity-stream UI** (`/stream`): the
  `7-stream-fold.html` direction rendered live — a stateless BFF **SSE proxy**
  over rp's `GET /api/events/subscribe` (which landed after this plan was
  written: 512-event replay, `Last-Event-ID` cursor, `stream_gap`) pushing
  server-rendered feed cards + status-strip slots into htmx `sse-swap`
  regions, on the pattern the `test-sse` spike proved.
- Deviations from the original sketch: the join is by config `id` (not ASCOM
  `UniqueID` — rp's roster is the source of truth and already id-keyed;
  `UniqueID` keying returns with discovery pre-fill); per-device
  connect/disconnect buttons are deferred until rp grows those endpoints
  (its registry is startup-static — the endpoints are *(planned)* in `rp.md`);
  ASCOM UDP discovery pre-fill stays deferred as designed.

## Open questions

- ~~**BFF crate name** — `ui` / `console` / `web`?~~ **Resolved:** `ui-htmx`,
  the first member of a `ui-*` family (tech-qualified for browser expressions,
  target-qualified for native). See [The BFF service](#the-bff-service-servicesui-htmx).
- **`server.port` changes** carry a cross-service reference: rp's roster
  `alpaca_url` for that device must change too. Out of scope while the BFF talks to
  the driver directly; revisit with the rp equipment page (Phase 5). **Interim:**
  the BFF renders `server.port` read-only so a change can't silently lock the page
  out of the driver (the BFF would keep using the old `base_url`) before that
  coordination exists.

### Resolved (2026-05-24)

- **No-`--config` drivers** → **write to the platform config directory**
  (e.g. `~/.config/rusty-photon/<service>.json` on Linux), not refuse. A path is always
  resolvable, so editing is never disabled; startup/reload read it if present.
- **No-auth drivers** → **all surfaces work; no per-action gate.** Config
  actions match ASCOM Alpaca semantics — they are protected by the server-wide
  `rp-auth`/`rp-tls` layer, which is an orthogonal concern from the action's
  behaviour. (Previously proposed: refuse without auth.)
- **Effective vs. file config** → **distinguish layers.** `config.get` returns
  the effective config *and* marks CLI-override-pinned fields (`overrides[]`);
  `config.apply` persists every field except those, so a transient `--port`
  cannot be baked into the file.

### Decisions (2026-05-27)

Calls made while scoping the [federated roster](#federated-roster-managed-own-vs-foreign-devices):

- **`pa-falcon-rotator` wraps the Falcon *v1* only; the *v2* stays
  vendor/firmware-managed.** The v2 ships its own Alpaca server + `falcon2.local`
  dashboard + Unity, so we do **not** author a v2 wrapper — re-wrapping would
  duplicate firmware we don't own. If the v2 surfaces in the UI at all it is a
  *foreign* device: controlled over its native Alpaca interface (manual entry),
  configured by deep-linking its vendor page — never via our `config.*`.
- **Per-server credentials, not a global one — including for drivers we author.**
  Every `ServerRef` carries its own `{auth, ca_cert}`; there is no guarantee that
  even our own drivers share a single credential. (The BFF's existing
  per-`DriverTarget` `auth`/`ca_cert_path` already points this way; the roster
  keeps it per-entry rather than collapsing to one shared credential.)
- **Manual `IP:port` entry is the primary path; UDP discovery is low-priority
  pre-fill only.** Discovery (the `ascom-alpaca` crate's `DiscoveryClient`,
  currently unused) won't cross subnets/VLANs, is "should" not "must" in the
  spec, and is best-effort — so it is deferred behind manual entry and, when
  added, only *merges* into the roster by `UniqueID`. The roster must never be
  gated on discovery.
- **Self-lockout guards must become a per-device rule set — refine empirically,
  not up front.** `dsd-fp2` hardcodes read-only `server.port` /
  `cover_calibrator.enabled` so the UI can't edit away its own reachability
  ([ui-htmx.md](../../services/ui-htmx.md) "Form ⇆ Config mapping"). This needs to
  generalise into a per-device declaration, but the right abstraction should be
  derived **after** `config.*` lands on a few more of our own drivers, so the rule
  shape is grounded in real cases rather than guessed. Future work for Phase 3.
- **Multiples are first-class; the mount is the only singleton.** The roster and
  control panels handle 1..N cameras / focusers / rotators / filter wheels (and
  several complete optical trains on one mount); only the mount is
  one-per-observatory. Optical-train grouping is a rusty-photon overlay from `rp`'s
  config — Alpaca has no such concept, just a flat device list — so trains are
  never inferred from discovery. (See
  [Federated roster](#federated-roster-managed-own-vs-foreign-devices).)
- **Publish `config.*` as a small standalone crate once proven.** The Phase 3
  shared helper graduates from a workspace-internal module to a vendor-neutral,
  independently publishable crate (convention constants + request/response types +
  optional `config.schema` + a thin helper to implement the actions on any
  `ascom-alpaca` `Device`), so third parties adopt the convention cheaply and
  auto-detect as *managed*. Timing: only after the pattern has settled across a few
  of our own drivers — don't publish a moving target.

### Decisions (2026-07-10)

Calls made while implementing Phase 4:

- **One supervised-services registry, top-level.** The restart endpoint must
  not require configuring the watchdog (whose block demands `rp_url`), and two
  maps carrying `restart_command` would drift — so `operation_watchdog.services`
  moved to a **top-level `services` map** consumed by both. Breaking config
  change, sanctioned pre-1.0.
- **Recovery confirmation is a shell command, symmetric with the restart.**
  Since `restart_command` is a shell command, the recovery probe is the
  optional per-service `health_command` (exit 0 = healthy) rather than an HTTP
  Alpaca probe — it works for non-Alpaca services and needs no device-type
  config. Follow platform best practice: `systemctl --user is-active <svc>` on
  unix, `sc query <svc> | findstr RUNNING` on Windows. The watchdog ladder's
  HTTP probe (device `connected`) is unchanged.
- **The restart budget is per-service.** `max_restart_duration` (default 60s)
  lives on each `services` entry and bounds command + recovery together, for
  both consumers; the watchdog-global `max_restart_duration` is removed — one
  knob per service.
- **Domain outcomes are HTTP 200.** A failed restart command is
  `{"status":"failed"}` on a 200 (mirroring `config.apply`'s
  `status:"invalid"` convention); 4xx is reserved for addressing errors
  (404 unknown name, 409 not-restartable / already in flight).
- **Manual restarts notify no one.** The endpoint logs, but does not dispatch
  notifiers or write notification history — the operator pressing the button
  already knows.

## Doc impact (CLAUDE.md rule 2)

When implementing: add a "Config actions" section to `docs/services/dsd-fp2.md`
(done); create `docs/services/ui-htmx.md` for the BFF (done); extend
`docs/services/sentinel.md` with
`service.restart` (Phase 4 — done: §Service Restart API + the top-level
`services` registry); note any `ReloadSignal` trigger addition in
`docs/skills/service-lifecycle.md`; and link this plan from `mocks/README.md`.

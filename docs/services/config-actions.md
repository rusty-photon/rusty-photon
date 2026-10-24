# Config actions — the cross-driver configuration protocol

> **Being retired.** [driver-setup-pages.md](../plans/driver-setup-pages.md)
> replaces these actions with the two HTML setup pages the Alpaca
> specification defines, served by each driver itself and rendered by the
> shared [`rusty-photon-setup`](../crates/rusty-photon-setup.md) crate.
> `dsd-fp2`, `pa-scops-oag`, `pa-falcon-rotator`, `ppba-driver`,
> `upbv2-driver`, `star-adventurer-gti`, `filemonitor`,
> `sky-survey-camera`, `svbony-camera`, `zwo-camera`, `qhy-camera` and
> `zwo-focuser` have made the move and no longer carry the actions;
> the other drivers follow family by family, and this document goes with
> the last of them.

Every rusty-photon Alpaca driver not yet on its setup pages exposes its own configuration over HTTP as three
**vendor ASCOM `Action`s** — `config.get`, `config.apply`, and `config.schema` —
so a single web UI can read, edit, and apply any driver's config without the
driver-specific knowledge living in the UI. This is the generalisation of the
Phase 1/2 `dsd-fp2` protocol (see
[`docs/plans/archive/config-actions.md`](../plans/archive/config-actions.md))
to **all** drivers.

The driver-agnostic machinery lives in the **`rusty-photon-config`** crate's
[`actions`](../../crates/rusty-photon-config/src/actions.rs) module; each driver
supplies only its specifics through one trait.

## The `ConfigurableDriver` trait

A driver implements this trait (in its `config_actions.rs`) for a zero-sized
marker type; everything else is generic free functions over it:

```rust
pub trait ConfigurableDriver {
    type Config: Serialize + DeserializeOwned + JsonSchema;   // the driver's config
    type Overrides;                                           // CLI-override carrier (`()` if none)

    fn normalize(config: &mut Self::Config);                  // trim/canonicalize before validate
    fn validate(config: &Self::Config) -> Vec<FieldError>;   // domain validation (empty = valid)
    fn secret_pointers() -> &'static [&'static str];         // RFC-6901 secret leaves to redact
    fn override_paths(overrides: &Self::Overrides) -> Vec<String>;       // dotted, CLI-pinned
    fn apply_overrides(config: &mut Self::Config, overrides: &Self::Overrides);
    fn locked_paths() -> &'static [&'static str] { &[] }     // identity fields (unlock-to-edit)
    fn read_only_paths() -> &'static [&'static str] { &[] }  // hard read-only (self-lockout)
    fn apply_disposition() -> ApplyDisposition { ApplyDisposition::Reload } // how changes take effect
    fn device_sections(config: &Self::Config) -> Vec<DeviceSection> { vec![] } // the setup pages' per-device sections
}
```

The trait is shared with the setup pages: `device_sections` names the config
section each served device's page edits, and the pages ignore the two tier
lists, which exist for `config.schema` alone.

Two details of the trait surface:

- **Wildcard secret pointers.** A secret pointer segment may be `*`, meaning
  "every element of the array (or key of the object) at this position" — e.g.
  `/equipment/cameras/*/auth/password` redacts the password of every configured
  camera. Patterns are expanded against the concrete config value at
  redact/persist time; exact (no-`*`) pointers behave as before. When a
  round-tripped sentinel is carried forward on persist, an array element is
  paired with its on-disk prior **by identity, not by index**: a `*` over an
  array re-locates the element whose `id` member matches the submitted one, so
  a submission that removes or reorders sibling entries never pairs a sentinel
  with another entry's stored secret. A submitted `id` with no on-disk match is
  treated as a rename when it is the *only* change to the array's id sequence
  (same length, every other position's id pairs up) — the entry keeps its
  stored secret; in any other case there is no prior and the apply fails loudly
  with `status:"invalid"` (the same error as a sentinel with nothing stored),
  never a silent cross-pairing. Elements without a string `id` keep positional
  pairing (no driver ships such a shape today).
- **`ApplyDisposition`.** `Reload` (the default, used by all six drivers) means
  changed fields take effect via in-process reload: they are reported in
  `reload[]` with `status:"applying"`. `Restart` is for services with **no**
  reload path (`rp`): changed fields are reported in `restart_required[]` with
  `status:"ok"` — persisted, honest that they only take effect on the next
  process start.

The generic functions — `config_get::<D>`, `config_apply::<D>`, `config_schema::<D>`
— implement the invariant protocol: secret redaction, layer-aware persist,
effective-config diff, and schemars JSON-Schema generation. They return plain
values / `ApplyError`, so **`rusty-photon-config` carries no `ascom-alpaca`
dependency** — it is the transport-/consumer-agnostic config *model*, shared with
`rp` (over its config REST route) and the plain-REST `sentinel` service.

The ASCOM **adapter** — wrapping those results into `ASCOMResult`, the generic
`config.get` / `config.apply` / `config.schema` action dispatch, the
`ConfigActionCtx`, and the shared transport-driver error model — lives in the
separate [`rusty-photon-driver`](../../crates/rusty-photon-driver) crate (which
*does* depend on `ascom-alpaca`, used only by the six driver services). Each
driver delegates `Device::action` / `Device::supported_actions` to
`rusty_photon_driver::dispatch::<D>` / `rusty_photon_driver::supported_actions`,
and defines its error type with the `rusty_photon_driver::driver_error!` macro —
so the dispatch, the `ApplyError → ASCOMError` mapping, and the common
`DriverError` variants each exist in exactly one place. See
[ADR-007](../decisions/007-rusty-photon-driver-shared-crate.md).

## The three actions

```
GET  /api/v1/{type}/{n}/supportedactions  → [..., "config.get", "config.apply", "config.schema"]

PUT  /api/v1/{type}/{n}/action   Action=config.get
   → Value = "<{ config: <effective, secrets redacted>, overrides: [dotted CLI-pinned] }>"

PUT  /api/v1/{type}/{n}/action   Action=config.schema
   → Value = "<{ schema: <JSON Schema>, locked_fields: [dotted], read_only_fields: [dotted] }>"

PUT  /api/v1/{type}/{n}/action   Action=config.apply   Parameters=<full Config JSON>
   → Value = "<{ status: applying|ok|invalid, applied[], reload[], restart_required[],
                 skipped_override[], persisted_to, errors[] }>"
```

Each `Value` is a JSON **string** inside the standard Alpaca envelope (the BFF
unwraps the envelope, then parses the string). The wire types are defined once in
`rusty_photon_config::actions` and reused by the BFF.

The three names are matched without regard to ASCII case, as the Alpaca
specification requires of every `Action` name (`CONFIG.GET` dispatches as
`config.get`); `SupportedActions` lists the canonical lowercase spelling.

### `config.apply` sequence

1. Parse `Parameters` into the driver's typed `Config` (parse failure →
   ASCOM `INVALID_VALUE` — a *transport* error, distinct from a domain error).
2. `normalize`, then `validate`; on failure or a redacted-secret-without-prior,
   return **HTTP 200** `{ status:"invalid", errors:[…] }`, file untouched.
3. **Layer-aware persist** (atomic temp→fsync→rename→fsync-dir): write every
   field *except* CLI-override-pinned ones (those carry through from the file's
   prior value, listed in `skipped_override[]`), and carry forward a redacted
   secret (the `********` sentinel means "keep the stored secret").
4. Diff the new effective config against the running one; the changed paths go in
   `reload[]`. Status is `applying` if anything changed (the driver fires the
   in-process reload **after** the response flushes), else `ok`.

Step 3 is a **full replace** from the submitted config, not a merge, so the
persisted file is whatever the typed config serialises to. That is why every
optional field a service persists carries
`#[serde(skip_serializing_if = "Option::is_none")]`: without it an apply
re-materialises every unset field as an explicit `null`, and a key the operator
deleted comes back on the next apply. Unset is spelled by the key's absence
(ADR-016 amendment 8), which each service pins in its own
`persisted_config_shape` unit test. Clearing a field over the wire is still an
explicit `null` — that is the only submitted spelling that means "unset this",
since an empty string is a different (often invalid) state — and it lands on
disk as an absent key.

### In-process reload

Drivers run under `ServiceRunner::with_reload().run_with_reload(...)` (see
[`docs/skills/service-lifecycle.md`](../skills/service-lifecycle.md)). A
`config.apply` that needs a reload fires `ReloadSignal::notify()` after a short
delay (so the HTTP response flushes first); the run loop breaks its
shutdown-or-reload stop future, the old server drains HTTP and releases its
transport, and the loop rebuilds from the freshly-persisted file — rebinding the
same port. The BFF treats `status:"applying"` as "expect a brief blip; reconnect
and re-`config.get`".

## REST transport (`rp`)

`rp` is not an ASCOM device, so it exposes the **same three operations as plain
REST** on its existing axum router — same request/response bodies, no Alpaca
envelope (the body is the JSON directly, not a JSON string inside `Value`):

```
GET  /api/config          → 200  ConfigGetResponse    { config, overrides }
GET  /api/config/schema   → 200  ConfigSchemaResponse { schema, locked_fields, read_only_fields }
PUT  /api/config  <body = full Config JSON>
                          → 200  ConfigApplyResponse  { status, …, restart_required[], … }
                          → 400  (malformed JSON body — the transport-error equivalent
                                  of the drivers' ASCOM INVALID_VALUE)
```

`rp` implements `ConfigurableDriver` with `ApplyDisposition::Restart`: it has no
in-process reload, so every changed field lands in `restart_required[]` with
`status:"ok"` and takes effect on the next `rp` start. Secrets (per-device
`auth` credentials across the equipment arrays, the server auth hash) are
redacted with wildcard pointers. The endpoints are covered by rp's server-wide
auth/TLS like every other route, and are **not** behind the `/mcp` safety gate —
config must stay editable while the system is unsafe. See
[`rp.md`](rp.md) "Configuration API".

## Editability tiers

JSON Schema cannot express identity/read-only intent, so `config.schema` returns
the tiers alongside the schema. The web UI evaluates them in precedence order:

| Tier | Source | UI |
|------|--------|----|
| Override-pinned | `config.get`'s `overrides[]` (CLI flags) | disabled; never persisted |
| Hard read-only | `read_only_paths()` | disabled; never editable (e.g. `server.port`, a device `enabled` flag) |
| Locked / identity | `locked_paths()` | disabled behind an "unlock to edit" escape hatch (e.g. a device `unique_id`) |
| Editable | everything else | enabled |

**Self-lockout guards:** a `server.port` change would make the driver rebind a
port the BFF can't follow; disabling a device tears down the very endpoint the
config actions live on; a `unique_id` is driver-owned (minted at startup by
the `rusty_photon_config::resolve_and_init` bootstrap). These are read-only /
locked so the UI can't edit away its own reachability.

## Driver coverage

| Driver | Devices | Secrets | Notes |
|--------|---------|---------|-------|
| `qhy-focuser` | Focuser | auth password hash | single device; the BFF's BDD vehicle, so it keeps the actions until P4 retires the BFF's config pages |
| `rp` | — (REST transport) | per-device `auth` passwords across the equipment arrays (wildcards) + server auth hash | `ApplyDisposition::Restart` — no reload; `server.port` read-only |

## The web UI

The `ui-htmx` BFF ([`ui-htmx.md`](ui-htmx.md)) is the browser-facing consumer: it
calls `config.get` / `config.schema` to render a form and `config.apply` to save,
reusing the `rusty_photon_config::actions` wire types. See `ui-htmx.md` for the
rendering and multi-driver routing. The schema walker and the form coercion it
renders with are `rusty-photon-setup`'s, shared with the drivers' own pages.

`dsd-fp2`, `pa-scops-oag`, `pa-falcon-rotator`, `ppba-driver`, `upbv2-driver`,
`star-adventurer-gti`, `filemonitor`, `sky-survey-camera`, `svbony-camera`,
`zwo-camera`, `qhy-camera`, `zwo-focuser` and `phd2-guider`'s camera facade no longer carry the actions: their `SupportedActions` list no `config.*` (the mount keeps
its `ApPark` actions, the facade its sweep lease) and they configure themselves on their own setup
pages (`phd2-guider` on its service listener's `/setup`, where the facade's setup paths redirect) ([`dsd-fp2.md`](dsd-fp2.md) "Setup Pages" describes the shape); the
BFF's roster links there.

## References

- Protocol design + phasing: [`docs/plans/archive/config-actions.md`](../plans/archive/config-actions.md)
- Protocol model (no ASCOM dep): [`crates/rusty-photon-config/src/actions.rs`](../../crates/rusty-photon-config/src/actions.rs)
- ASCOM adapter (dispatch + `driver_error!` macro + error model): [`crates/rusty-photon-driver`](../../crates/rusty-photon-driver) — see [ADR-007](../decisions/007-rusty-photon-driver-shared-crate.md)
- Reload lifecycle: [`docs/skills/service-lifecycle.md`](../skills/service-lifecycle.md)
- Per-driver specifics: each `docs/services/<driver>.md` "Config actions" section.

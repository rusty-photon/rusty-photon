# Driver-served setup pages

**Status:** accepted (merged 2026-10-17,
[#1538](https://github.com/rusty-photon/rusty-photon/pull/1538)); P1 merged
([#1542](https://github.com/rusty-photon/rusty-photon/pull/1542)); P2 merged
([#1545](https://github.com/rusty-photon/rusty-photon/pull/1545)); P3 under
way: the serial family merged
([#1549](https://github.com/rusty-photon/rusty-photon/pull/1549)), the
`GTi` merged
([#1557](https://github.com/rusty-photon/rusty-photon/pull/1557)),
`filemonitor` and `sky-survey-camera` merged
([#1558](https://github.com/rusty-photon/rusty-photon/pull/1558)), the
camera family merged in three PRs: `svbony-camera`
([#1613](https://github.com/rusty-photon/rusty-photon/pull/1613)) with the
shared crate's driver hook (D11, settled with the owner on 2026-10-22),
`zwo-camera`
([#1614](https://github.com/rusty-photon/rusty-photon/pull/1614)) and
`qhy-camera`
([#1615](https://github.com/rusty-photon/rusty-photon/pull/1615));
`zwo-focuser` merged
([#1616](https://github.com/rusty-photon/rusty-photon/pull/1616));
`planetarium-bridge` under way (the Phases table has the state). On the owner's direction the servers are
specification-compliant from the first phase, and the `ascom-alpaca`
crate is fixed properly — its setup hooks take a request (D10, P1); the
owner decided on 2026-10-17 that the fork's fixes are **not offered
upstream for now**. D8 and D10 are settled by the owner (2026-10-16) and
P1 implements them; the owner merged the plan and said to go on, so P2
builds on the recommendations D1–D7 state, and the plan is revised in
place where P2 found a detail to settle (D2's note on the trait).

## Goal

Every rusty-photon Alpaca server configures itself through the two HTML
pages the Alpaca specification defines for the purpose, served by the
driver itself behind the same TLS and Basic Auth as everything else it
serves:

- `/setup` — the whole server: its port and discovery settings, its
  transport, the devices it serves, and the username and password it
  requires;
- `/setup/v1/{device_type}/{device_number}/setup` — one device: its
  name, description and the settings only it has.

`rp`'s equipment roster page links to those pages, one link per entry.
The `config.get` / `config.apply` / `config.schema` custom actions, their
REST twin on `rp` (`/api/config`), and the schema-driven forms `ui-htmx`
renders from them are retired. Custom actions stay what the
specification intends them for: device-specific features, and the
runtime protocols between our own services (the facade lease, the mount's
park actions).

The operator's path becomes: open the driver's page, set what the driver
needs — the serial device, the port, which USB cameras it serves, the
username and password it demands — then open `rp`'s roster page and
enter that address and credential on the entry that uses the driver.

## Context

### What exists today

Every driver but `planetarium-bridge` exposes three vendor ASCOM
`Action`s on each of its devices
([config-actions.md](../services/config-actions.md)). `config.get`
returns the effective config as a JSON string with secrets replaced by a
sentinel; `config.schema` returns a JSON Schema plus two lists of dotted
paths (`locked_fields`, `read_only_fields`) that the schema cannot
express; `config.apply` takes a whole config, validates it, persists it
layer-aware (CLI overrides written through from the file), classifies
the changed paths, and fires the in-process reload after the response
flushes. `rp` serves the same three operations as plain REST. Each
service implements one trait, `ConfigurableDriver`, in its
`config_actions.rs`.

`ui-htmx` is the only consumer. It calls the three actions through the
device `rp`'s roster names, walks the schema into a flat list of scalar
leaves, renders a form, carries everything it cannot render — every
`oneOf`/`anyOf`/`enum`, every array of objects, every secret — through a
hidden JSON blob, overlays the submitted leaves, and submits the whole
config back. After a save it polls `config.get` until the reloaded
driver answers. Fields a save could use to cut the page off from the
driver (`server.port`, each device's `enabled`) are rendered read-only;
a device's `unique_id` sits behind an "unlock to edit" hatch.

The footprint, by the survey of 2026-10-16 (production lines unless
stated):

| Piece | Where | Size |
|---|---|---|
| The protocol: trait, wire types, get/apply/schema, wildcard secret pointers, override write-through | `crates/rusty-photon-config/src/actions.rs` | 692 (+1024 test) |
| The ASCOM adapter: `ConfigActionCtx`, `dispatch`, the error mapping | `crates/rusty-photon-driver/src/{actions,error}.rs` | 170 |
| Per-service `config_actions.rs` | 15 services (13 drivers, `phd2-guider`, `rp`) | 2841 total |
| Per-service `config_actions.feature` | 13 drivers, 90 scenarios | — |
| Per-service step code for them | 13 copies, none shared | 2403 |
| `rp`'s REST handlers and their feature | `services/rp/src/routes.rs`, `config_rest.feature` (11 scenarios), `config_rest_steps.rs` | ~55 + 442 |
| The schema walker, renderer, tiers and merge | `services/ui-htmx/src/pages/mod.rs` | ~1040 (+970 test) |
| The action client, the config handlers, the reconnect poll | `services/ui-htmx/src/{driver_client,lib}.rs` | ~680 |
| `ui-htmx`'s features on it | `config_page` 8, `rp_config_page` 6, `restart` 6, `browser` 4 | 24 scenarios |

Beyond the lines, the design carries rules that exist only because a
JSON blob travels between a driver and a browser through a third
process: the redaction sentinel and its by-`id` pairing on persist; the
editability tiers; the self-lockout list each driver declares; the
hidden blob and its forgery caveat; the unbounded reconnect poll; the
placeholder camera's generic parameter, carried only so a missing camera
still serves the actions; and `config.apply`'s refusal of an empty
`usb_devices` list, because an empty list would unregister every device
the actions are reachable through.

### Why that is the wrong shape

**1. The specification already defines where this belongs.** The Alpaca
API Reference (version 12, 23 February 2026, §4.1) defines "a main HTML
browser URL that acts as the primary user entry point for the whole
Alpaca device" at `/setup` and "a dedicated URL for each ASCOM Device
presented by the Alpaca Device so that ASCOM Device specific
configuration can be set", at `/setup/v1/{device_type}/{device_number}/setup`,
"in a similar fashion to the COM SetupDialog method". Of the server page
it says: "This could be a good place to enable the astronomer user to
change the Alpaca discovery port number and any other 'whole device'
configuration settings." The Management API's OpenAPI description is
stronger: the page "must also enable the user to set cross cutting
information that applies to the whole device such as the operating IP
address and port number."

Custom actions, by contrast, are described as "a standardised means for
drivers to extend functionality beyond the built-in capabilities of the
ASCOM device interfaces", whose "key advantage … is that drivers can
expose any device specific functionality required". Every action is
addressed to a device: `/{device_type}/{device_number}/action`,
"Invokes the named device-specific action." A server's configuration is
not a device's feature.

**2. Clients already reach those pages.** The ASCOM Remote client's
setup dialogue — the form the Platform's Alpaca Dynamic Clients
inherited — has an "Alpaca Web Configuration" group with two buttons: "The 'Whole Alpaca Device' button displays the main page for
the whole Alpaca device (regardless of how many ASCOM devices it is
presenting), while the 'This ASCOM Device' button displays a
configuration page dedicated to the ASCOM device" (ASCOM Remote
Installation and Configuration, v2.0, §2.1.7). A NINA or SGPro user on
Windows is one click from a driver's page, and from nothing of ours.

**3. Third-party servers do exactly this.** `rig2` runs Optec's
FocusLynx Alpaca server on port 4247, and `rp`'s roster there names it
(`focuslynx`). Its `/setup` is one page with sections for the server
(remote access, port), discovery, security (require authorization,
user name, password, repeat), and adding or removing focusers; its
`/setup/v1/focuser/0/setup` redirects to its own `/focuser/0` control
page; its `SupportedActions` lists dozens of device features
(`SetTempComp`, `SetNickname`, `SetBacklashCompSteps`, …) and no
configuration protocol. That is the shape the specification describes,
and it needed no second process to render it.

**4. The BFF path does not work on the deployment target.** On a
doctor-wired rig every service requires the observatory credential.
`ui-htmx` calls drivers with no credential — its config carries no field
for one, and doctor writes its credential only for its `rp` and
`sentinel` targets — so on that rig every first-party driver probes as
"auth required" and gets no Configure link. The page's own hint already
says what to do instead: "configure it from its own setup UI or its
config file". The setup UI did not exist.

**5. A device is the wrong anchor for a server's configuration.** Every
self-lockout rule follows from reaching the server's config through one
of its devices: a device's `enabled` flag cannot be edited, a
`usb_devices` list cannot be emptied over the wire, a placeholder must
serve the actions, and a driver that serves no device at all has no page.
`/setup` belongs to the server and exists whether or not it serves a
device, so those rules dissolve. The remaining case — a changed
`server.port` moves the page — is a message, not a lockout.

**6. The intermediary is where the complexity lives.** Everything in the
table above exists to move a config between a driver and a browser
through `ui-htmx`. With the driver rendering its own form, the wire
protocol, the redaction sentinel, the tiers, the blob, the client, the
poll and the fifteen per-service adapters go, and what remains is one
shared renderer and one shared handler behind the crate's hooks.

### What stays

- **The reload.** `ReloadSignal` and the run loop live in
  `rusty-photon-service-lifecycle`, which knows nothing of the actions;
  SIGHUP and the SCM `ParamChange` control reach it the same way a saved
  form will.
- **The persist model.** `rusty_photon_config::save` (atomic
  temp→fsync→rename→fsync-dir), layer-aware persist (CLI-override-pinned
  fields written through from the file), absence as the spelling of
  "unset" (ADR-016 amendment 8), and each service's typed `Config` with
  its validation. Only the wire protocol around them goes.
- **Doctor** (ADR-016). It repairs files on disk and never used the
  actions. The recovery model keeps its shape: the page is served by the
  service's own HTTP server, so a service that cannot start has no page,
  and the repair path stays `ssh` plus `doctor --fix`.
- **TLS and Basic Auth** (ADR-002, ADR-003). `rp_auth::layer` wraps each
  driver's whole router, Alpaca fallback included, and TLS wraps the
  listener; the crate's setup routes, and so our hooks behind them, are
  covered with no change.
- **Sentinel.** Process restarts stay its job.
- **The runtime custom actions**: `phd2-guider`'s facade lease (whose
  client is `rp`'s train lease, an MCP tool pair, untouched),
  `star-adventurer-gti`'s park actions, and the vendor features any
  driver adds later.

## Decisions

### D1. The two pages and what each holds

`/setup` is the server's page. It holds every section of the config
that is not one device's: `server` (port, bind address, discovery port,
TLS paths, auth), the transport (`serial`, `transport`), the
service-wide `usb_devices` list, and a list of the devices the server
serves, each linking to its own page. A section that belongs to one
device — a camera's `devices.{id}` override among them — is edited on
that device's page alone, never here. The crate's own `/setup` (a
static device list) is replaced by ours.

`/setup/v1/{device_type}/{device_number}/setup` is one device's page:
the config section that device is built from (`cover_calibrator`,
`focuser`, `switch`, `rotator`, `mount`, a camera's `devices.{id}`
override …) — name, description, `enabled`, `unique_id`, and the
settings only it has (`max_brightness`, `averaging_period`, …). A device
the config has no section for (a camera served from the no-list default)
renders what the driver knows about it and links to the server page.
A placeholder's page says which port it stands in for and links to the
server page's `usb_devices` section.

Both pages answer `GET` with the form and `POST` with the save (D3). The
crate, once fixed (D10), routes both specification paths for `GET` and
`POST`, links to the device path from its `/setup`, and redirects its
old non-spec path; our pages answer through the crate's hooks.

**Recommendation:** as stated. The split follows the specification's;
FocusLynx uses it, and the ASCOM simulators' pages follow the same shape
(one server page, a setup page per device).

### D2. One generated implementation, hooks for the rest

The pages are generated from each service's config type, as
`ui-htmx`'s forms are today, not hand-built per driver. `ui-htmx`'s
schema walker and renderer (`FieldModel`, `FieldKind`, the sorted
object walk, the integer-enum checkbox group, the coercion of
submissions) move into a new shared crate, `rusty-photon-setup`, which
renders with Maud and depends on neither `ascom-alpaca` nor any service.
The same crate carries the handler behind the pages — the `GET` render,
the `POST` save (D3), the origin check (D7) and the reload hook — as
one function that a device's `setup_request`, a `Server`'s setup
handler (D10), and `rp`'s, `ui-htmx`'s and `phd2-guider`'s own routers
call. No route of ours is merged ahead of the crate's: the crate routes
both specification paths, and a driver answers them through the hooks.
`rusty-photon-driver` keeps only what is Alpaca-specific — the glue
from a device's config context to the shared handler.

The walker grows two things the drivers need that `ui-htmx` never did:
a tagged enum whose variants are plain objects (`star-adventurer-gti`'s
`transport`, `usb` or `udp`) renders as a selector with the chosen
variant's fields, and **a field the walker still cannot render is named
on the page as file-only** — never silently absent, which is what the
old read-only list did for the GTi transport.

`ConfigurableDriver` stays as the per-service seam but changes shape:

- kept: `Config`, `Overrides`, `normalize`, `validate`,
  `override_paths`, `apply_overrides`, `apply_disposition`;
- kept and re-purposed: `secret_pointers` — a secret leaf renders as an
  empty password input whose blank submission means "unchanged", and is
  never written into the page;
- new: `device_sections()` — which config pointer each served device's
  page edits, so the per-device page knows its section;
- new, optional: `custom_sections()` — a driver-specific section the
  generic walker cannot render, with its own Maud fragment and its own
  parse of the submitted fields. The camera drivers' `usb_devices`
  editor (D11) is the first;
- dropped: `locked_paths` and `read_only_paths`. A `unique_id` renders
  under a collapsed "Identity" disclosure, editable; `server.port` is
  editable, and the save's response says where the page now lives (D3).

*As built in P2:* `device_sections` takes the config (a camera driver's
devices depend on its `usb_devices` list) and defaults to none, so the
fourteen implementors still on the actions compile unchanged; the two tier
lists stay on the trait until P5, because `config.schema` reads them, and
the pages ignore them. `custom_sections` lands with its first user, the
camera family's `usb_devices` editor (D11, P3). *As built in P3:* the hook is not a trait method but a `DriverPages`
the driver hands its `SetupPages` (`with_driver_pages`), because the
camera family's part reads what the running server registered and a
live USB scan, which `ConfigurableDriver`'s static functions cannot
reach. It names the top-level keys its sections edit (the walker leaves
them off every page), the devices the server serves when the
configuration alone cannot name them, and its sections on each page,
each with its markup, its parse and an optional confirmation
([rusty-photon-setup.md](../crates/rusty-photon-setup.md), "A driver's
own part"). The tagged-enum selector
is built and unit-tested in the crate (one radio button per variant, the
chosen variant's fields shown by CSS, no script); `star-adventurer-gti`
proves it on a page in P3. A page shows the configuration file (through
the typed config, so a default shows) with the pinned fields as the running
service has them — the base the next save overlays — and a `Restart`
service's page names the saved fields still waiting for its restart.
Disabling a device from its page sends the browser to the server page,
since the crate serves no page for an unregistered device; re-enabling it
is a file edit until a phase decides otherwise.

**Alternative:** hand-built Maud pages per driver, as FocusLynx has. They
would look better and cost fifteen pages that drift. **Recommendation:**
generated, with the custom-section hook for the few places a form
needs more than fields. A driver can replace the generated page with a
hand-built one later without changing the hooks.

### D3. Saving

A `POST` to either page submits that page's fields. The handler:

1. reads the file as the base (not the effective config: CLI overrides
   are pinned on the page and never persisted, as today), overlays the
   submitted fields onto the page's section, and leaves every other
   section as the file has it — a save of one device's page cannot
   alter the server section;
2. parses the result as the typed `Config`, then `normalize` and
   `validate`; a failure re-renders the form with the error beside its
   field and the file untouched;
3. persists with `rusty_photon_config::save`, override-pinned fields
   written through from the file;
4. answers with a page that says what was saved, and
   - for a `Reload` service, that the service is reloading. A reload
     drops the listener and binds again only after the driver's
     handshake, so the page cannot simply refresh once — a refresh that
     lands in that window is a browser error page with nothing left to
     refresh. A few lines of inline script poll the page's own URL until
     it answers — any HTTP answer, a `401` after a credential change
     included, means the listener is back — then navigate to it;
     without script, a refresh header after a delay longer than a
     typical reload and a link do the same by hand. The reload is fired after the response flushes, as today
     (`RELOAD_AFTER_RESPONSE_DELAY` moves to the shared handler);
   - for a `Restart` service (`rp`, `phd2-guider`), which fields take
     effect on the next start, and how to restart it (D12);
   - when `server.port` or the bind address changed, the new URL — as a
     link for a `Reload` service, and beside the restart text for a
     `Restart` service, where nothing answers at it until the restart.

Nothing a `GET` does changes state.

*Owner's decision (2026-10-17, from P2's review):* a saved `server.port`
or bind address that the rebuilt listener cannot bind — a port in use, a
privileged port for an unprivileged service, an address the host does
not have — is the operator's risk, accepted as is. The save checks the
number's range only; the rebuilt server fails to bind, the reload loop
returns the error, the process exits, and the unit restarts it on the
same file until someone edits the file over the shell (a bad port can
also be overridden by starting the service with `--server-port`; the
bind address has no flag). D2 makes the port editable, and the owner
prefers the power with the responsibility to a page that cannot move
the port. (The old `read_only_paths` had held `server.port` for a
different reason — the BFF could not follow a rebind — which the saved
page's new URL answers; the self-lockout rules and why they dissolve
are point 5 of §Why that is the wrong shape.) No bind probe before the
save, and no fallback to the previous configuration in the reload
loop, are owed; a shared reload loop built in P3 may revisit the
fallback.

### D4. `rp`'s roster page is `rp`'s own

`rp` serves `/setup` too: its own config, with the equipment roster as
the page's centre. Each entry shows its address, its device number, its
live connection state (which `rp` holds), the credential `rp` presents
to it (D6), and two links: the driver's server page and that device's
page (D5). Adding, editing and removing entries are forms on this page,
generated per kind from the entry types, as `ui-htmx`'s are today. The
REST `/api/config` routes are retired with the drivers' actions.

`rp`'s `plugins` registry is an array of opaque objects — a
registration is the plugin author's vocabulary — which no generic walker
renders and no page of `ui-htmx` edits today; its only wire path was
`PUT /api/config`. After P4 it stays edited in the file, which `rp`'s
page says beside its list of the registered plugins; a typed plugins
section is a custom section (D2) for a later item. With `/api/config`
gone, a plugin's credential reaches no page.

`rp` is not an Alpaca device, so the specification does not oblige it;
the argument is uniformity. One rule — *a service's configuration is
edited on that service's `/setup`* — is simpler to hold than one rule
for drivers and another for `rp`, and it is where the operator looks for
the roster after configuring a driver. `rp.md`'s design tenet 7 ("UI
is a client, not a component") is about application logic, which a
config form is not; its tenet 8 already carves config out of the
MCP-only rule ("plus config, which must stay reachable while conditions
are unsafe"). Sentinel already serves its own dashboard behind the same
layers.

The rule's reach under this plan: every Alpaca server — the thirteen
drivers, `planetarium-bridge`, and `phd2-guider`'s facade (D13) — and
the services the plan touches anyway, `rp`, `ui-htmx` and
`phd2-guider`. The other core services with a listener and a config
file (`sentinel`, `plate-solver`, `session-runner`, `calibrator-flats`,
`focus-model`, `polar-align`) stay file-edited and doctor-wired; once
the shared crate exists, a `/setup` for each is a small later item, not
one of these phases.

**Alternative:** `ui-htmx` keeps `/equipment` and `/config/rp` over
`rp`'s REST config, and only gains the links. It is the smaller change
and keeps the schema renderer in two places (the shared crate and
`ui-htmx`), the REST transport, the by-`id` secret pairing and the
hidden blob — for one service. **Recommendation:** `rp` serves its own.
`ui-htmx` keeps the session surfaces (the stream, the targets inbox) and
links to `rp`'s page from its navigation.

### D5. Links to a driver's pages, from a loopback address

A roster entry's `alpaca_url` on the single-box rig is
`https://127.0.0.1:<port>`; a browser on the operator's laptop would
follow that to itself. The link `rp`'s page renders therefore keeps the
entry's scheme and port and substitutes the host when the entry's host
is a loopback address (`127.0.0.1`, `::1`, `localhost`): the host the
browser used to reach `rp`'s page, taken from the request's `Host`
header without its port. Any other host is used as written.

On a doctor-issued certificate the hostname is a SAN, so the browser's
only complaint is the observatory CA, imported once; an IP address is a
SAN only when doctor was given `--extra-san`, and the page says so when
the substituted host is an IP. Under ACME the wildcard `*.<domain>`
covers any `<name>.<domain>` the operator reached `rp` by. The same
rule applies to the Setup-page link `ui-htmx`'s equipment page renders
from P2 until P4 retires that page, to its navigation link to `rp`'s
page, and to a third-party server's link, as with `rig2`'s `focuslynx`
entry. The one server the rule cannot reach is one that listens on
loopback alone; today that is `phd2-guider`'s facade (D13).

### D6. Credentials

A driver's `/setup` has a **Security** section: username, password,
repeat. A submitted password is hashed (Argon2id, `rp-auth`) by the
driver and persisted as `server.auth.password_hash`; the hash never
reaches a page, and a blank password with a username means "keep the
stored hash". A username with no stored hash and a blank password is
refused with the field named — turning auth on needs a password — so
`server.auth` is never persisted without its hash. Clearing the
username turns auth off. After a save that
turned auth on or changed the password, the reload answers the page's
own refresh with a `401`, and the browser asks for the new credential:
the same path the sentinel dashboard has always used.

`rp`'s roster page carries each entry's `auth` block (username,
password) as fields: the password stored in plaintext, as ADR-003 has it
for every client, rendered as an empty password input, blank meaning
"unchanged".

Doctor's minted observatory credential (ADR-016 decision 10(e)) stays
the zero-touch default: `doctor --fix` fills absent blocks, on both
sides, and a credential set on a page is the "operator intent" doctor
already treats a present block as. The two paths coexist as they do
today for a hand-edited file, with one consequence the page states:
doctor's provisioning writes `server.auth` wherever it is absent
(ADR-016 decision 10 — auth on by default), so on a doctor-wired rig
clearing the username lasts only until the next `doctor --fix`. Running
without auth is a choice for a rig doctor does not wire, not a page
setting; the plan asks nothing of doctor. Two gaps found on the way are
recorded below, not fixed here:
`doctor auth rotate` overwrites every `server.auth` without
redistributing the plaintext, and `doctor.md` disagrees with itself on
whether `rp` holds the observatory credential.

### D7. Cross-site protection

Browsers resend cached Basic credentials on any request to the origin,
so a form `POST` behind Basic Auth is open to cross-site request forgery:
a page on another site could submit a driver's form with the operator's
credential. Every `POST` is therefore accepted only when the request
says it came from the page itself: the `Sec-Fetch-Site` header is
`same-origin` or `none`, or, where a browser sends none, the `Origin`
header's host and port are the request's own. A `POST` failing the check
is refused with `403` and a sentence saying why. No token, no cookie,
no state.

### D8. What is retired, what is kept

Retired, service by service as each gains its pages (P2 and P3) and in
the final pass (P5):

- `config.get`, `config.apply`, `config.schema` from every
  `SupportedActions`; `ConfigActionCtx`, `dispatch`, the
  `ApplyError → ASCOMError` mapping; each service's
  `with_config_actions`; `config_actions.feature` and its steps;
- the wire types and the redaction sentinel in
  `rusty-photon-config::actions`, with its wildcard-pointer expansion
  and by-`id` pairing (`save` and the layer-aware persist stay, used by
  doctor and by D3);
- `rp`'s `/api/config` and `/api/config/schema`, `config_rest.feature`;
- `ui-htmx`'s `/config/*` routes, `AlpacaConfigClient`,
  `RestConfigClient`, the schema renderer (moved, D2), the reconnect
  poll, the Restart-via-Sentinel card, `/equipment` and its forms,
  `config_page`, `rp_config_page`, `restart` and `equipment_page`
  features; the `browser.feature` scenarios move their vehicle to the
  stream page;
- the placeholder camera's `ConfigurableDriver` parameter and
  `claims::empty_list_over_apply` / `apply_errors` (an empty list is
  saved on the server page like any other);
- [config-actions.md](../services/config-actions.md), archived with the
  2026-05 plan it documents; the "Config actions" section of every
  service doc, replaced by a "Setup pages" section.

Kept: every runtime custom action, and `Action` itself. In P1 the names
are matched case-insensitively, as the specification requires ("Action
names must be case insensitive, so for example SelectWheel, selectwheel
and SELECTWHEEL all refer to the same action"); today the config, the
lease and the park actions all match exactly. `SupportedActions` keeps
returning each name in its canonical spelling, which the specification
also requires.

### D9. `ui-htmx` after this plan

`ui-htmx` keeps what is a UI over the running system — the activity
stream, the equipment LEDs in the stream's strip, the targets inbox —
and drops every configuration page. Its navigation gains a link to
`rp`'s `/setup` (D5's host rule). It no longer needs Sentinel's
`probe_port` or the restart endpoint; whether the `sentinel` block
stays for a future dashboard link is D12's. Its own config is then
edited on its own `/setup`, served by the same shared crate (`ServerConfig`
plus `rp`'s address), so the rule in D4 holds for it too.

### D10. The `ascom-alpaca` fork: a complete, specification-compliant setup surface

The pinned crate — and upstream's `main`, which registers the same five
routes — serves the device setup page at `/api/v1/{type}/{n}/setup`, a
path no specification defines, as a special case inside the action
route; its `/setup` page links there; and the specification's
`/setup/v1/{device_type}/{device_number}/setup` is not routed at all.
The Device API says `SetupDialog` "has been omitted from the Alpaca
Device API because it presents a user interface rather than returning
data" and defers configuration to the Management API, which defines
exactly two setup paths: `/setup` and
`/setup/v1/{device_type}/{device_number}/setup`. So every server built
on the crate, ours included, answers `404` where a spec-following client
(the Remote client's "This ASCOM Device" button, `ui-htmx`'s own probe)
looks.

The hook behind the page is a stub. `Device::setup(&self)` returns an
HTML string, is served on `GET` alone, and defaults to "Sorry, this
device does not support configuration"; no service of ours overrides
it. The crate's own doc comment says how a page is meant to save — "You
can use `Self::action` to store the configuration" — which is a
per-device form of the pattern this plan retires, and one a plain HTML
form cannot use, since `action` is a `PUT`. The crate has no hook at all
for the `/setup` page: it renders a static device list.

**Settled by the owner on 2026-10-16: the crate is fixed properly,
before any page of ours is built.** Two changes on one fork branch, both
landed in P1 — and, on the owner's decision of 2026-10-17, **not offered
upstream for now**: the fork branch `feature/spec-setup-pages` carries
both, with their tests, and the split below is how they go upstream as
two pull requests when the owner says so:

1. **The paths and statuses** — small and plainly right:
   - a `GET /setup/v1/{device_type}/{device_number}/setup` route — the
     Management API says this page "must be implemented, even if the
     response to the user is that the device is not configurable", which
     the crate's default page already is — answering `403` with an HTML
     message for an unknown device type or number, the status that API
     gives the path ("The provided path specifies an unsupported device
     or method");
   - the `/setup` page links to that path;
   - `/api/v1/{type}/{n}/setup` answers with a permanent redirect to the
     spec path, so a client that followed the old link is not stranded;
     a later crate release removes it;
   - the action route's answer for an unknown device number or an
     unknown action becomes `400`, which the API Reference's status-code
     examples require ("Camera device 1 does not exist", "'CanSlew' is
     not a valid Camera command", §2.5.2); the crate answers `404` today.
     It is the one change in the set a client can see, so ConformU's
     Alpaca-protocol checks run against a driver on the fork before the
     pin moves. The owner confirmed this item separately on
     2026-10-16.
   - a test per route and per status, in the crate's own suite.
2. **Saving through the hooks** — a pull request of its own, since the
   shape of the API is the maintainer's call:
   - a request-taking device method added beside the old one, so no
     implementor breaks: `setup_request(&self, request)`, the request
     carrying the method, the headers, the query and a parsed form body,
     the response an HTML page or a redirect. Its default answers `GET`
     with today's `setup()` and `POST` with `405`. The crate routes `GET`
     and `POST` on the spec device path to it;
   - a setup handler on `Server` for `/setup`, with the same request and
     response, defaulting to today's device list, routed for `GET` and
     `POST` — the page the specification says "must also enable the
     user to set cross cutting information";
   - the crate already parses form bodies for `PUT` actions, so the
     plumbing exists; a test per method and per default.

Our drivers then implement the hooks rather than merging routes ahead
of the crate's: each device's `setup_request` and the server's handler
delegate to the shared crate's handler (D2), which renders, checks the
request's origin (D7), saves (D3) and fires the reload. A device without
a page of ours — none, after P3 — gets the crate's default, and so does
every other user of the crate. The workspace pin moves to the fork
branch carrying both pull requests — a `Cargo.lock` change, so
`MODULE.bazel.lock` is repinned (rule 10) — and
[workspace.md](../workspace.md)'s note on the pin names #23 and the
setup-page change, and says which has been offered upstream. If upstream
merges, the pin follows upstream as the existing one is meant to; nothing
of ours waits on it.

### D11. The camera drivers' `usb_devices` editor — C8 lands here

The device-claims plan's deferred C8 (`ui-htmx` editing of
`usb_devices`) becomes a custom section (D2) on each camera driver's
`/setup`. It is the design settled with the owner on 2026-10-16:

- one row per port the driver's vendor has a camera on now (the passive
  USB scan the driver already runs, no SDK call, nothing opened), plus
  every listed port with no camera on it (a placeholder's row);
- a checkbox per row: checked means served, with its device number,
  name, description, and in `qhy-camera` the declared wheel number; a
  newly checked row takes the next free number; unchecking any but the
  highest-numbered camera renumbers the rows after it, as the list's
  0..N-1 rule requires, and the page shows before the save which
  numbers move and says that `rp`'s entries bound to those numbers must
  be renumbered to match — `rp`'s own page (P4) shows which entries
  those are, the driver cannot — because an entry without a `unique_id`
  pin would otherwise open the wrong camera with no error;
- a mode switch between "serve every camera automatically" (no list)
  and "serve only the checked cameras" (a list); all boxes unchecked is
  the empty list, saved like any other — the server page outlives it;
- the rows do not name the `rp` entry bound to each number: that join
  is `rp`'s, and shows on `rp`'s page.

The two problems that held C8 — the page was reachable only through a
device, and the empty list cut it off — do not exist on a server page.

*Settled with the owner for P3 (2026-10-22):*

- **Three PRs**, one driver each: `svbony-camera` with the crate's hook and
  the shared editor, then `zwo-camera`, then `qhy-camera` with the declared
  wheel number.
- **A camera's own page follows the mode.** A listed camera's page shows
  what the driver knows about it and links to its row on the server page,
  where its name and description are edited, so each field has one place.
  A camera served without a list edits its `devices` override's name and
  description, and the first save that names either creates the override.
  A placeholder's page is D1's.
- **The renumbering is confirmed, not previewed by script.** A save that
  would move a served camera's number answers with the moves and the
  warning about `rp`'s entries, and writes nothing until the operator saves
  that page again; the pages stay without script beyond the reload wait.
- Changing the mode carries the names across: to a list, each camera's
  override names fill its row and the `devices` map goes; to no list, each
  listed camera's names become an override under its key. Any name the
  change would drop is named in the same confirmation.
- From the first PR's reviews, on the owner's word: the confirmation also
  covers a number a dropped camera held passing to another camera, and
  names an unchecked entry's names it drops, every row unchecked included.
  With no list after a save, the numbers are checked against the port
  order the reload would give the bus as the page's scan finds it now — a
  record new since the start taken for a camera, since the page makes no
  SDK call — so a camera plugged in ahead of the others is confirmed too.

*As built in `zwo-camera` (P3's second PR):* an ASI camera's serial, the
key of its `devices` override, is read only by opening it, and with a list
the start opens only the listed cameras. So the cameras the build tells
the shared pages it placed carry their key only where it was read: every
camera served with no list, and with a list each listed camera whose open
succeeded. Leaving a list confirms the names of a listed camera whose open
failed as dropped, as it does a placeholder's. A placed camera the start
did not open — unlisted, or its open failed — has no key, and whether the
reload opens it the page cannot tell, so the confirmation of a save that
leaves no list names those cameras, lists the moves for none and for all
of them opening where they differ, and states the port-order rule for any
other outcome (the PR's first two reviews, rated fix). `max_adu_reporting`,
a string enum the generic form does not render, is file-only on the page,
as the mount's `tracking_rate` is.

*As built in `qhy-camera` (P3's third PR), settled with the owner on
2026-10-22:* each row of the Cameras section has a wheel box, which
declares the camera's filter wheel, and the page numbers the checked
wheels as it numbers the cameras — they keep the order of their wheel
numbers, a newly checked one takes the next, and a save that moves one is
confirmed, with the warning about `rp`'s filter wheel entries. Unchecking a
camera takes its wheel with it. The filter names are edited on each wheel's
own page, `/setup/v1/filterwheel/{n}/setup`, one name per line: in the
camera's entry with a list, in its `devices` override without one; a mode
change carries them across with the names. A QHY camera's key is its SDK
id, read without an open, so every placed camera's is known, and whether
the no-list reload serves a camera is a flag of its own beside the key: the
QHY SDK does not refuse an open of a camera another application holds, so
a list's unlisted cameras are taken as served, and only a camera a no-list
start's CFW probe could not open or close is served only if the reload's
opens it. Without a list the reload serves the wheels its probe finds,
which the page cannot tell, so a save that leaves a list declaring a wheel
is always confirmed, saying so.

### D12. Restart

Reload services need no restart affordance. For the two `Restart`
services, the saved page names the fields that wait for the next start
and says how to restart (`systemctl restart rusty-photon-rp`, the SCM,
`brew services`). Sentinel's dashboard gains a restart button per
service row — it has had the `POST /api/services/{name}/restart`
endpoint since the 2026-05 plan's phase 4 and no button — so the
operator who prefers a click has one, on the service that owns
restarts. That button makes the endpoint a form target behind cached
Basic credentials, so the same origin check as D7's goes on that `POST`
in Sentinel's router. No page links to Sentinel, because no driver knows where it
is. **Recommendation:** as stated; the alternative, a `sentinel_url` on
every service, is a cross-reference ADR-016 spent a plan removing.

### D13. `phd2-guider`

`phd2-guider`'s only Alpaca server is the camera facade, which is off by
default and, by the device-claims plan's C6, listens on a loopback
address alone — any other bind address is refused at load. The rule is
there because doctor's catalog describes each service with one port; a
second port that exists only when `camera.enabled` is true would raise
false collision and availability findings whenever it is off, and the
catalog shape for an optional listener is follow-up work that plan
deferred. A `/setup` on that server would be unreachable from the
operator's browser and absent whenever the facade is off, and D5's host
rule would render a link to a port nothing answers on.

So `phd2-guider`'s `/setup` is served by its always-on service listener,
the one its REST API and `/health` use, and covers its whole config, the
`camera` block included. The facade keeps its loopback-only rule; its
hooks (D10) answer both spec paths with a redirect to that page, which
is right for the one place the facade is reachable from, the host. On
`rp`'s roster page the facade's entry gets no device link: `rp` knows
the entry is the facade (its `SupportedActions` lists the lease action
`rp` already looks for) and knows the guider's service URL from its own
`guiding.url`, so the entry links the guider's `/setup` under D5's host
rule, as the guiding block itself does. Nothing in doctor's catalog
changes.

**Alternative:** lift the loopback-only rule, which is the optional-
listener catalog work C6 deferred. **Recommendation:** as stated; the
link `rp` can already compute makes the lift unnecessary for this plan.

## Consequences

- **Browser trust is per origin.** The operator's browser talks to N
  services instead of one. On a self-signed install that is one CA
  import, then no warnings; on ACME, none. Basic Auth prompts once per
  service origin per browser session, with the same credential
  everywhere on a doctor-wired rig. `ui-htmx` hid both costs by being
  the only origin; it also could not reach an authenticated driver.
- **Drivers render HTML.** Each gains `maud` (compile-time templates,
  no runtime cost) and two hook implementations. No JavaScript: the pages are plain
  forms, the only script is the reload wait's poll (D3) with a
  no-script fallback, and the look comes from one embedded stylesheet in
  the shared crate using the mocks' tokens. `rp.md`'s footprint tenet
  (5) is unchanged in kind — a page is a string.
- **The recovery model holds.** A service that cannot start has no
  page, as it had no actions; `doctor --fix` and the file remain the
  repair path (ADR-016, Consequences).
- **A breaking change, sanctioned pre-1.0.** `config.*` leaves every
  `SupportedActions`; no third-party client is known to use it. The
  2026-05 plan's intent to publish `config.*` as a vendor-neutral crate
  is withdrawn.
- **The fork carries one more change until upstream merges.** The pin
  already tracked a fork branch for upstream PR #23; P1 stacks the
  setup-page change on it, so the arrangement does not change in kind,
  and `workspace.md` records both — #23, offered upstream, and the
  setup-page change, not offered for now (D10) — until the pin can
  return to upstream. The hooks' shape is the maintainer's call, and a
  different shape upstream changes only the glue in
  `rusty-photon-driver`, not the shared handler.
- **ADR-016 is amended, and a new ADR records the rule.** ADR-016's
  recovery model ("config-actions tunes what runs; doctor repairs what
  won't start") keeps its substance with the page in the actions'
  place; a short ADR-023 records *services configure themselves on
  their own setup pages; custom actions carry device features and
  runtime protocols only*, and why.
- **Secrets never reach a page.** A hash or a client password renders
  as an empty password input; the hidden blob that today carries them
  to the browser is gone. (It carries more than it should: see below.)
- **Testing moves with the code.** One shared step library in
  `bdd-infra` replaces thirteen copies; each service's
  `setup_pages.feature` states its own contract (its sections, its
  reload, its secrets) in a few scenarios, and the renderer's own
  behaviour is pinned once, in the shared crate. The scenarios that used
  `config.apply` only as a way to trigger a reload
  (`star-adventurer-gti`'s connection lifecycle) submit the form
  instead.
- **Two documents go.** `config-actions.md` and its plan are archived;
  `ui-htmx.md` loses its config half.

## Phases

Small, each mergeable alone, in this order.

| Phase | Description | Status | Branch / PR |
|-------|-------------|--------|-------------|
| P0 | This plan; ADR-023 and the ADR-016 amendment once D1–D13 are settled | Plan merged; the ADRs wait on the decisions | `feature/driver-setup-pages` ([#1538](https://github.com/rusty-photon/rusty-photon/pull/1538)) |
| P1 | **Specification compliance first.** The `ascom-alpaca` fork, in the two changes D10 describes (not offered upstream yet, the owner's call): the paths and statuses — the specification's device setup path routed, linked from `/setup`, the old path redirected, the statuses the specifications give, with tests — and the request-taking hooks, `setup_request` on a device and a setup handler on `Server`, routed for `GET` and `POST`, with their defaults. The workspace pin moves to the fork branch, `MODULE.bazel.lock` repinned. Our `Action` names match case-insensitively (D8), with a scenario per action family. After P1 every driver answers both spec paths — with the crate's default pages until P2 | Merged | `feature/setup-pages-p1` ([#1542](https://github.com/rusty-photon/rusty-photon/pull/1542)) |
| P2 | **The shared crate and the first driver.** `rusty-photon-setup` (the walker and renderer moved out of `ui-htmx`, the stylesheet, the password-input and secret rules), the shared handler behind the pages (the render, the save, the origin check, the reload hook) and its tagged-enum and file-only rules, the shared BDD steps in `bdd-infra`; on `dsd-fp2`, the reference driver, whose device and server hooks (D10) call that handler, with its `config.*` removed in the same PR. `ui-htmx`'s capability probe keeps `config.*` first, so a driver still carrying the actions keeps its Configure form through P3; when `supportedactions` answers `401` or lists no `config.*`, the probe asks for the spec-path setup page and treats a `401` there as a page that exists (the browser will ask for the credential). A driver that loses the actions therefore gains the link in the same step, on an auth-on rig too, with the link's host following D5; on such a rig an unmigrated driver's link leads to the crate's default page until its family lands. `ui-htmx`'s own config-page suites move their vehicle from `dsd-fp2` to `qhy-focuser`, which still carries the actions, until P4 retires those pages | Merged | `feature/setup-pages-p2` ([#1545](https://github.com/rusty-photon/rusty-photon/pull/1545)) |
| P3 | **Rollout**, one PR per family, each driver's actions removed as its pages land: the serial drivers (`qhy-focuser`, `pa-scops-oag`, `pa-falcon-rotator`, `ppba-driver`, `upbv2-driver`, `star-adventurer-gti`); `filemonitor` and `sky-survey-camera`; the camera drivers with the `usb_devices` editor (D11) and the placeholder's page (`svbony-camera`, `zwo-camera`, `qhy-camera`); `zwo-focuser`; `planetarium-bridge`, which has no config actions and no reload today, so its item adds the seam the others have — a `ConfigurableDriver` for its config and `run_with_reload` — and its page then saves and applies like any driver's (Reload, unless the item finds its spool makes a restart the safer disposition); `phd2-guider` (D13: its `/setup` on the service listener; the facade keeps loopback and redirects there) | In progress | Serial family merged: `pa-scops-oag`, `pa-falcon-rotator`, `ppba-driver`, `upbv2-driver` ([#1549](https://github.com/rusty-photon/rusty-photon/pull/1549)). `star-adventurer-gti` merged on its own ([#1557](https://github.com/rusty-photon/rusty-photon/pull/1557)), the tagged-enum gaps below closed in the crate first. `filemonitor` and `sky-survey-camera` merged together ([#1558](https://github.com/rusty-photon/rusty-photon/pull/1558)): no overrides, so nothing pinned; `filemonitor`'s rules list and the camera's follow-mode blocks are file-only on the page. The camera family in three PRs (the owner's choice, 2026-10-22), each a driver with its `usb_devices` editor: `svbony-camera` merged first ([#1613](https://github.com/rusty-photon/rusty-photon/pull/1613)), with the shared crate's `DriverPages` hook and the editor and camera pages shared in `rusty-photon-driver`'s `claims` module; `zwo-camera` second ([#1614](https://github.com/rusty-photon/rusty-photon/pull/1614)), its placed cameras' override keys optional, since an ASI serial is read only by opening the camera; then `qhy-camera` ([#1615](https://github.com/rusty-photon/rusty-photon/pull/1615)), its declared wheels numbered by the page as the cameras are and each wheel's page editing its filter names. `zwo-focuser` merged ([#1616](https://github.com/rusty-photon/rusty-photon/pull/1616)): its devices are the EAFs the SDK enumerates, which the file only renames, so its own part of the pages is `rusty-photon-driver`'s `overrides` module — a page per EAF that edits its `devices` override, with the override editor the camera pages' unlisted camera now shares. `planetarium-bridge` (`feature/setup-pages-p3-planetarium-bridge`) reloads rather than restarts, once its spool was made safe across a stop: the import worker had stopped only at a bound and been abandoned past it, replaying on meanwhile, which a process exit ends but a reload would have left beside the next worker on the same spool file; it now stops after the delivery in flight and is aborted and waited for past the bound. Its `rp.service_auth` block and `assume_epoch` are file-only on the page, and the altitude floor's schema is an optional number, so clearing it saves the `null` that disables it. `qhy-focuser` is the BFF's BDD vehicle for its config pages and keeps the actions until P4 retires those pages. |
| P4 | **`rp`'s own `/setup`** (D4): the roster with live state, per-entry credentials (D6), the links with the host rule (D5), add/edit/remove; the REST config routes retired; `ui-htmx` drops its config pages and `/equipment`, links to `rp`'s page, and gets its own `/setup`. Sentinel's dashboard restart button (D12) | Not started | |
| P5 | **Retirement.** `rusty-photon-config::actions` reduced to what D3 uses; `config-actions.md` archived; every service doc's "Config actions" section replaced; README, packaging docs, `workspace.md`, `service-lifecycle.md` updated | Not started | |

*Closed in the `GTi`'s PR, for the first driver with a tagged-enum
section (`star-adventurer-gti`'s transport), from P2's review and the
serial family's:* a driver `validate` error on a variant's field carries
the config path (`transport.port`) while the control is named by the
variant (`transport:usb.port`) — the handler re-points it at the chosen
variant's control, so it renders beside the field rather than in the
banner; `with_secrets` reaches the fields a variant carries; a flag that
pins a field inside a variant (`--port`) pins that control alone, and one
that pins the enum (`--transport`) pins the selector and every field —
`ConfigurableDriver::pinned_paths` takes the config, because `--port`
lands on `transport.port` or `transport.address` by the kind in effect;
an unchosen variant's fields render their schema defaults, so a switch
starts from them rather than from blanks; saves are serialized per
service, from the file read to the persist; and the redirect after a
device page disables its device, like the server page's note under a
disabled device, reads the configuration the reloaded service runs, so a
device an `--enable-*` flag keeps on stays on its page and one the flag
keeps off is noted as the flag's doing, not the file's. From the `GTi`'s
own review: a driver names the fields it writes at runtime
(`file_only_pointers` — the park position `SetPark` captures, the AP-park
choices) and the page names them as file-only rather than carrying inputs
a stale form would write back over the device's write; and whether a save
changed anything is judged against the file as it was read, so a runtime
write since the start does not make a save that changes nothing reload.

P1 comes first and depends on no decision: it makes what we ship
compliant before anything new is built on it. P2 is the proof: one
driver whose config is read and written on its own pages, with the
actions gone and the equipment link working, before anything is rolled
out. P3's families can land in any order. P4 waits for P2 only; it is
ordered after P3 so `rp`'s page links to drivers that have pages.

## Testing

- **Shared crate:** unit tests on the walker and renderer move with the
  code (ui-htmx's 970 lines of tests are the start), plus the coercion
  and secret rules.
- **Per service, `setup_pages.feature`:** the server page shows the
  current values; a device page shows only its section; a valid save
  persists, reloads, and the page shows the new value after the wait; an
  invalid save names the field and leaves the file untouched; an
  override-pinned field is shown pinned and not persisted; no hash and
  no client password appears in any page; a cross-site `POST` is
  refused; with auth on, the pages answer `401` without a credential and
  render with one; a `server.port` change answers with the new URL. The
  camera drivers add the `usb_devices` section's scenarios (the mode
  switch, a pin, an unpin that renumbers, the empty list, a
  placeholder's row). The reload scenarios that used `config.apply`
  submit the form.
- **`rp`:** the roster page with its links (a loopback entry's host
  substituted, another host kept), per-entry credentials round-tripping
  blank-as-unchanged, add/edit/remove, and the restart-required text.
- **`ui-htmx`:** the probe's new order (a first-party driver with pages
  and no `config.*` gets the Setup-page link, behind auth too, from the
  `401`; its host follows D5), the navigation link, and the browser
  scenarios on the stream page.
- **On the rigs:** `pier1` (Linux, doctor-wired, TLS and auth on) and
  `rig2` (Windows, the FocusLynx entry): open each driver's page from a
  laptop, save a change, see the reload, set a credential and enter it
  on `rp`'s page. A validation record per rig.

## Found on the way

Recorded here so they are not lost; none is this plan's to fix unless
noted.

- **`rp`'s `GET /api/config` returns plugin passwords in plaintext.**
  `plugins[].auth.password` is a plaintext client credential and is not
  among `rp`'s `secret_pointers()`, so the response carries it and
  `ui-htmx`'s `rp` config page embeds it in the browser's hidden blob.
  A one-line fix today (add the pointer); the route goes in P4.
- **Every server built on `ascom-alpaca` answers `404` on the
  specification's device setup path** (`/setup/v1/{type}/{n}/setup`),
  the path `ui-htmx` probes and the Remote client opens; the crate
  routes the page at `/api/v1/{type}/{n}/setup` instead, upstream too.
  P1 fixes it in the fork (D10; not offered upstream yet).
- **`doctor auth rotate` rewrites `server.auth` on every service and
  sentinel's `service_auth`, nothing else**; `doctor.md` says it
  "re-runs the same distribution". `rp`'s equipment entries and
  `ui-htmx`'s targets keep the old plaintext.
- **`doctor.md` disagrees with itself** on whether `rp` holds the
  observatory credential (the client-wiring table says CA only; the
  `joins.client-auth` fix writes it into `rp`'s equipment entries).
- **`config-actions.md`'s coverage table is stale:** it lists nine
  services; fifteen implement the trait.
- **`ui-htmx.md` drift:** the restart callout's text, the two
  `equipment_page` scenarios it describes that do not exist, `RpApi`'s
  members; and `pages/mod.rs`'s comment says "three surface tabs" where
  the doc and the page have four.
- **Sentinel's dashboard has the restart API and no button.** P4.
- **The pages' origin check trusts the request's `Host`.** A page on
  another site that rebinds its name to the rig's address passes the
  check as the browser sees it (`Sec-Fetch-Site: same-origin`, or an
  `Origin` equal to the `Host` it chose), and with no `server.auth` the
  save is accepted — the same surface the unauthenticated Alpaca API on
  that listener has always had. The complement is a `Host` allow-list,
  which needs configured hostnames the config does not carry; not this
  plan's.

## References

- ASCOM Alpaca API Reference, version 12 (23 February 2026), §4.1 HTML
  Interfaces, §4.2.2 Description and Configured Devices —
  <https://ascom-standards.org/AlpacaDeveloper/ASCOMAlpacaAPIReference.html>
- Alpaca Management API, OpenAPI — the `/setup` page's obligations —
  <https://ascom-standards.org/api/AlpacaManagementAPI_v1.yaml>
- Alpaca Device API, OpenAPI — `action` and `supportedactions` —
  <https://ascom-standards.org/api/AlpacaDeviceAPI_v1.yaml>; the
  canonical `Action` / `SupportedActions` definitions —
  <https://ascom-standards.org/newdocs/camera.html>
- ASCOM Remote Installation and Configuration, v2.0 (2021), §2.1.7
  "Alpaca Web Configuration" —
  <https://download.ascom-standards.org/docs/RemoteInstConf.pdf>
- The design being retired:
  [config-actions.md](../services/config-actions.md),
  [ui-htmx.md](../services/ui-htmx.md), and the 2026-05 plan,
  [archive/config-actions.md](archive/config-actions.md)
- Config ownership, doctor, the credential:
  [ADR-016](../decisions/016-service-config-ownership-and-doctor.md);
  auth: [ADR-003](../decisions/003-authentication-for-device-access.md);
  TLS: [ADR-002](../decisions/002-tls-for-inter-service-communication.md)
- The `usb_devices` editor's origin: the device-claims plan's C8,
  [device-claims-and-phd2-camera.md](device-claims-and-phd2-camera.md)

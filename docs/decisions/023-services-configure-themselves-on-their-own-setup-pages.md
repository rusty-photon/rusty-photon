# ADR-023: Services configure themselves on their own setup pages

## Status

Accepted (2026-10-17, with the plan
[`docs/plans/archive/driver-setup-pages.md`](../plans/archive/driver-setup-pages.md),
[#1538](https://github.com/rusty-photon/rusty-photon/pull/1538)). Recorded
2026-10-25, once the plan's decisions D1–D13 were settled and its phases
P1–P5 had shipped.

Retires the `config.get` / `config.apply` / `config.schema` action protocol
([`config-actions-protocol.md`](../plans/archive/config-actions-protocol.md),
from the 2026-05 [config-actions plan](../plans/archive/config-actions.md)).
[ADR-016](016-service-config-ownership-and-doctor.md) amendment 9 carries
its recovery model over to the pages, and
[ADR-007](007-rusty-photon-driver-shared-crate.md) records that the action
dispatch it describes is gone.

## Context

From 2026-05, every rusty-photon Alpaca driver exposed its configuration as
three vendor ASCOM `Action`s on each of its devices, and `rp` served the
same three operations as REST. `ui-htmx` was their one consumer:
- it rendered a form from `config.schema`;
- it carried what it could not render through a hidden JSON blob;
- it submitted the whole config back through `config.apply`.

The plan sets out why that shape was wrong (§Why that is the wrong shape):

1. **The specification defines where configuration belongs.** The Alpaca
   API Reference (version 12, §4.1) defines two pages:
   - `/setup`, "a main HTML browser URL that acts as the primary user entry
     point for the whole Alpaca device";
   - `/setup/v1/{device_type}/{device_number}/setup`, a page for each
     device, "so that ASCOM Device specific configuration can be set".

   Custom actions, by contrast, are "a standardised means for drivers to
   extend functionality beyond the built-in capabilities of the ASCOM
   device interfaces", each addressed to one device. A server's
   configuration is not a device's feature.
2. **Clients already open those pages.** The ASCOM Remote client's setup
   dialogue has a button for the whole Alpaca device's page and one for the
   device's own page. A NINA or SGPro user on Windows was one click from a
   driver's page, and the page did not exist.
3. **Third-party servers already work this way.** The Optec FocusLynx
   server on rig2 has one `/setup` page with sections for its server,
   discovery, security and focusers. Its `SupportedActions` lists device
   features and no configuration protocol.
4. **The intermediary did not work on the deployment target.** On a
   doctor-wired rig every service requires the observatory credential, and
   `ui-htmx` held none for the drivers, so no first-party driver got a
   Configure link.
5. **A device was the wrong anchor for a server's configuration.**
   Reaching the server's config through one of its devices bred self-lockout
   rules:
   - a device's `enabled` flag could not be edited;
   - a `usb_devices` list could not be emptied;
   - a placeholder camera had to serve the actions;
   - a driver that served no device had no page at all.
6. **The intermediary was where the complexity lived.** Several pieces
   existed only to move a config between a driver and a browser through a
   third process:
   - the redaction sentinel and its by-`id` pairing;
   - the editability tiers;
   - the hidden blob;
   - the reconnect poll;
   - the fifteen per-service adapters.

## Options Considered

### Option 1: Keep the actions and the BFF, with the credential gap closed

This fixes point 4 alone. The protocol, the blob and the self-lockout rules
stay, and the pages the specification defines and its clients open still do
not exist.

### Option 2: Each service serves its own pages, generated from its config type (chosen)

The specification's two pages, served by the service itself behind its own
TLS and Basic Auth, are rendered and saved by one shared crate from the
service's typed configuration. Everything in points 5 and 6 goes. What
remains is one renderer, one save handler and a small per-service trait.

### Option 3: Hand-built pages per driver

FocusLynx's approach. The pages would look better, but they would be
fifteen pages that drift apart (plan D2). A service can still replace its
generated page with a hand-built one later, through the same hooks.

## Decision

1. **A service's configuration is edited on that service's own setup
   pages.**
   - Every Alpaca server serves the two pages the specification defines:
     the thirteen drivers and `planetarium-bridge`.
   - `phd2-guider` serves its `/setup` on its always-on service listener.
     Its camera facade's setup paths redirect there, because the facade
     listens on loopback alone and only when enabled (plan D13).
   - `rp` and `ui-htmx` serve their own `/setup`. `rp`'s holds its
     equipment roster, with links to each driver's pages.

   Each page sits behind the service's own TLS and Basic Auth. `rp` is not
   an Alpaca device, so the specification does not oblige it to serve a
   page. The reason is uniformity: one rule is simpler to hold than one rule
   for drivers and another for `rp` (plan D4).
2. **The pages are generated by one shared crate, `rusty-photon-setup`.**
   It holds the render, the save, the origin check and the reload. A
   service supplies its own half:
   - `rusty_photon_config::driver::ConfigurableDriver`: its config type,
     validation, secrets, pinned fields and device sections;
   - a `DriverPages`, for sections that fields alone cannot draw, such as
     a camera driver's `usb_devices` editor.
3. **Custom actions carry device features and runtime protocols only.**
   These are `phd2-guider`'s facade lease and any vendor feature a driver
   adds later. No action carries configuration. A page may also carry an
   operator command for its device, which is never part of a save: the
   mount's recovery page shows what the mount is doing and carries a
   Stop and the recovery from a known pose, in place of the AP-park
   actions it retired.
4. **A stored secret never reaches a page.** A password hash or a client
   password renders as an empty password input, and a blank submission
   keeps the stored value.

## Consequences

- **The recovery model holds** (ADR-016 amendment 9). A service that
  cannot start has no page, as it had no actions. `doctor --fix` and the
  file stay the repair path.
- **Browser trust is per origin.** The operator's browser talks to each
  service rather than to one:
  - a self-signed install needs one CA import, and ACME needs none;
  - Basic Auth prompts once per service origin per browser session, with
    the same credential everywhere on a doctor-wired rig.
- **A save says how it takes effect.** A service that reloads in process
  says so, and its saved page waits for the service to come back. A service
  that restarts instead (`rp`, `phd2-guider`, `ui-htmx`) names the fields
  waiting for the next start and says how to restart it. Sentinel's
  dashboard has a restart button per service (plan D12).
- **The `ascom-alpaca` fork carries one more change** (plan D10): the
  crate's setup hooks take the request, and it routes both specification
  paths. On the owner's decision (2026-10-17), the change is not offered
  upstream for now.
- **A breaking change, sanctioned pre-1.0.** `config.*` left every
  `SupportedActions`, and `rp`'s `/api/config` is gone. The 2026-05 plan
  intended to publish the protocol as a vendor-neutral crate; that intent
  is withdrawn.
- **Six services have no page yet:** `sentinel`, `plate-solver`,
  `session-runner`, `calibrator-flats`, `focus-model` and `polar-align`.
  They stay file-edited and doctor-wired, and a `/setup` for each is a
  small later item on the shared crate.

## References

- [`docs/plans/archive/driver-setup-pages.md`](../plans/archive/driver-setup-pages.md) —
  the plan, decisions D1–D13 and phases P0–P5
- [`docs/crates/rusty-photon-setup.md`](../crates/rusty-photon-setup.md) —
  the pages, the save and the reload
- [`docs/crates/rusty-photon-config.md`](../crates/rusty-photon-config.md)
  — the crate that holds `ConfigurableDriver`
- [`config-actions-protocol.md`](../plans/archive/config-actions-protocol.md)
  — the protocol this ADR retires
- [ADR-002](002-tls-for-inter-service-communication.md) and
  [ADR-003](003-authentication-for-device-access.md) — the TLS and Basic
  Auth each page sits behind
- [ADR-016](016-service-config-ownership-and-doctor.md) — config
  ownership, doctor, and the recovery model (amendment 9)
- ASCOM Alpaca API Reference, version 12 (23 February 2026), §4.1 — the
  two setup pages and custom actions
- ASCOM Remote Installation and Configuration, version 2.0, §2.1.7 — the
  client's "Alpaca Web Configuration" buttons

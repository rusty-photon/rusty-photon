# `rusty-photon-setup` Crate Design

The HTML setup pages every rusty-photon service serves for its own
configuration: the form a page renders from the service's config type, the
save a `POST` to it performs, and the rules both follow. The pages are the
ones the Alpaca Management API defines — `/setup` for the whole server and
`/setup/v1/{device_type}/{device_number}/setup` for one device — served by
the service itself behind its own TLS and Basic Auth
([driver-setup-pages.md](../plans/driver-setup-pages.md)).

This is a workspace library, not a service. It depends on neither
`ascom-alpaca` nor any service: an Alpaca driver reaches it through
`rusty-photon-driver`'s glue, which turns the pinned crate's
`SetupRequest` into this crate's [`PageRequest`](#requests-and-responses)
and back; a plain HTTP service calls the handler from its own router.

## Scope

- Walk a service's config schema (`schemars`) into the fields a form
  renders, and coerce a submitted form back into config values.
- Render the two pages: the server page (every section that is not one
  device's, the Security section, the device list) and a device page (that
  device's section alone).
- Save: overlay a submission onto the page's section of the *file* config,
  parse it as the typed config, normalize and validate, persist atomically,
  answer with what was saved, and fire the service's reload after the
  response has flushed.
- Refuse a cross-site `POST`.
- The Security section: a username and password that become
  `server.auth` with an Argon2id hash, never a page that shows a hash.

What it leaves to the service: the config type and its validation
(`ConfigurableDriver` in `rusty-photon-config`), which config section each
device edits (`device_sections`), which fields a command-line flag pins,
which fields the service itself writes at runtime (`file_only_pointers`),
where the file lives, and — for a driver whose configuration the walker
cannot render, or whose devices its configuration alone cannot name — its
own part of the pages ([A driver's own part](#a-drivers-own-part)).

## The pages

Both pages answer `GET` with the form and `POST` with the save. Each is one
HTML document with its stylesheet inline (the dark theme and tokens of the
`ui-htmx` mocks), no script except the reload wait's and the tagged-enum
switch's, and a form whose `action` is the page's own path, so the page
works at any mount.

**The server page** holds, in this order:

1. every top-level section of the config that no device page edits
   (`serial`, `server`, a service-wide `usb_devices` list, …), each a
   `<fieldset>` with the section's fields, a `unique_id` among them under
   a collapsed **Identity** disclosure as on a device page (`phd2-guider`'s
   `camera` block, whose device has no page of its own);
2. **Security**: the server's username, a password and its repeat. See
   [Credentials](#credentials);
3. **Devices**: one row per device section the config has, naming the
   device and linking to its page. A device whose section is disabled
   (`enabled: false`) is listed with a note that its page is not served
   while it is disabled and that the file is where to enable it — or,
   when a command-line flag pins the section's `enabled`, that the flags
   are; the crate it sits behind serves no page for a device it did not
   register.

**A device page** holds that device's section alone: its name,
description, `enabled`, the settings only it has, and its `unique_id` under
a collapsed **Identity** disclosure, editable — the driver owns the id and
mints it, so editing it is an escape hatch, not routine. A device whose
section carries no `enabled` or `unique_id` field renders without those.
The page links back to the server page.

A page shows the configuration **file** — what the service runs once it
has reloaded, and the base the next save overlays — read through the typed
config so a field the file leaves at its default shows that default (and
its schema `default` when the typed config does not write it back either:
a section its type leaves out of the file while it is all defaults, whose
own default can differ from its fields' and wins while a save leaves the
section as shown), with
a field a command-line flag pins showing what the running service uses:
disabled, with a note that says so, and never persisted by a save (see
[Saving](#saving)). Which fields the flags pin is the driver's
`ConfigurableDriver::pinned_paths(config, overrides)`, read against the
running config because a flag can land on one field or another by the
config's shape (the `GTi`'s `--port` is the serial path on a USB
transport and the host on a UDP one); it defaults to the driver's
`override_paths`. A service that restarts rather than reloads
(`ApplyDisposition::Restart`) names, above the form, the saved fields
still waiting for its restart, and how to restart it. A file that cannot be read shows the
running configuration instead.

### Fields

The walker turns the schema into scalar leaves, each a form control named
by its dotted path (`serial.port`), grouped by top-level section, in sorted
key order so the rendered order is the same on every host:

| Schema shape | Control | Coercion of the submission |
|---|---|---|
| `string` | text input | verbatim; an optional (`["string","null"]`) string cleared persists `null`, a required one persists `""` |
| `boolean` | checkbox | present ⇒ `true`, absent ⇒ `false` |
| `integer` with optional `minimum`/`maximum` | number input, the bounds its `min`/`max` | parsed and range-checked; out of range or not a number is a field error; an optional one cleared persists `null`, a required one cleared is a field error |
| `number` with optional `minimum`/`maximum` | number input taking any value (`step="any"`: without it a browser accepts only whole steps from the value the input starts at), the bounds its `min`/`max` | parsed and range-checked; not a finite number (`inf`, `NaN`, which JSON cannot hold) or out of range is a field error; an optional one cleared persists `null`, a required one cleared is a field error |
| `array` of an integer `enum` | one checkbox per enumerated value, in schema order | the checked values, in schema order; none checked ⇒ `[]` |
| a plain object | recursed | — |
| `$ref` into `$defs` | resolved; a `default` beside the `$ref` is the field's (a newtype field's `serde(default)`), else the definition's own | — |
| `Option<T>` where `T` is an object (`anyOf [T, null]`) | file-only (below) | — |
| an internally tagged enum whose variants are objects (`oneOf` of objects sharing one `const` property; `schemars` renders a newtype variant around a struct this way too) | one radio button per variant, with the chosen variant's fields shown and the others hidden by CSS; each variant's fields sit in a fieldset that a few lines of script in the group disable while it is not chosen, so the browser neither checks nor sends a hidden variant's fields and a value typed there before switching away cannot block the save (without script every fieldset stays on); an unchosen variant's fields start from their schema `default`s, so a switch to it is the selector plus the fields it requires | the chosen variant's fields, under its tag; the other variants' submitted fields are ignored. A variant's field is a control named by the variant (`transport:usb.port`): a driver `validate` error naming it by its config path (`transport.port`) is shown beside that control; a pinned path inside the enum pins that control alone, a pinned enum pins the selector and every field; `secret_pointers` reach a variant's fields |
| anything else (`allOf`, `enum`, `const`, arrays of objects, custom-serde types) | file-only | — |

**File-only** is never silent: the page names the field, says it is edited
in the configuration file, and the save leaves it as the file has it. A
rule a rejected save attaches to such a field is shown in its note.
`server.tls` (certificate and key paths) is file-only on every driver;
doctor writes it. A service also names the fields it writes itself at
runtime (`file_only_pointers`: a mount's park position, captured by
`SetPark`), whatever the schema made of them: a form filled before such a
write would otherwise carry the old value back on its next save. The same
list holds a trust setting a page must not choose: rp's `ca_cert`, the CA it
trusts for every credential it presents, is file-only as `server.tls` is.

Labels are humanised from the path's last segments (`max_brightness` →
"Max brightness").

### Requests and responses

The handler takes a `PageRequest` — the method, the headers, the query
string's pairs and the form body's pairs, both in order with repeats kept,
so a checkbox group arrives whole — and answers a `PageResponse`: a status,
headers and a body. The statuses:

| Situation | Status |
|---|---|
| `GET` | `200` with the form |
| `POST` that saved | `200` with the saved page |
| `POST` whose values were rejected | `422 Unprocessable Content`, the form again with each error beside its field and the submitted values kept; an error on a field this page does not show (the file's other section failing validation) is named above the form, with its path |
| `POST` whose save a driver's section asks to confirm first (a renumbering), or that would remove a bound credential ([Credentials](#credentials)) | `409 Conflict`, the form again with the submitted values, what the save would change, and the button that saves it ([Confirmation](#confirmation)) |
| `POST` that fails the origin check | `403` with a sentence saying why |
| `POST` whose save failed (the file could not be written, the file on disk is not valid JSON) | `500` with the reason |
| a device page for a device the config has no section for | `404` with a sentence; the crate in front answers `403` first for a device it does not serve |

## Saving

A `POST` to a page submits that page's fields. The handler:

1. reads the config **file** as the base (a missing file is the running
   config; a file that is not valid JSON refuses the save, so a typo is
   never overwritten), and overlays the submitted fields onto the page's
   section alone — a save of a device page cannot alter the server section,
   and a save of the server page cannot alter a device's. A field of a
   section the file leaves out starts that section only when the
   submission changes it from what the page showed: a section left alone
   stays out of the file, for the typed config to fill from the section's
   own default, which can differ from its fields' own. A section a change
   starts begins from the values the typed config fills for it, so a field
   the page does not send (a pinned one) is kept, or empty for a section
   the typed config leaves out of the file while it is all defaults, which
   the parse completes from its fields' defaults. The credential block
   starts its section the same way. It then has each of
   the driver's own sections on the page apply its part
   ([A driver's own part](#a-drivers-own-part));
2. skips every field a command-line flag pins: the file's value stays;
3. parses the result as the typed config, then `normalize` and `validate`;
   a failure re-renders the form with each error beside its field and the
   file untouched;
4. asks the driver's sections whether the change needs confirming, and
   whether it would remove a bound credential
   ([Credentials](#credentials)), and answers `409` with what it would
   change when either does and the submission does not confirm it
   ([Confirmation](#confirmation));
5. persists with `rusty_photon_config::save` (atomic, owner and mode kept);
6. answers with a page that says what was saved, and
   - for a service that reloads in process (`ApplyDisposition::Reload`),
     that it is reloading: the reload is fired after the response has
     flushed, and the page waits for the listener to come back. Whether
     anything changed is judged against the file as it was read (with the
     overrides applied), not the running configuration: a runtime writer
     (a mount's `SetPark`) may have moved the file on since the start, and
     a save that changes nothing must not reload for it. A few
     lines of script poll the page's own URL until any HTTP answer arrives
     — a `401` after a credential change included — then navigate to it
     (a `no-cors` probe, since after a port change the URL is another
     origin); without script, a `Refresh` header after a delay longer than
     a typical reload and a link do the same by hand. A save that removed
     a bound credential has neither: its page stays on the removal, with
     the link ([Credentials](#credentials)). A save that changed nothing
     says so and fires no reload;
   - for a service that needs a restart (`ApplyDisposition::Restart`),
     which fields take effect on the next start and how to restart it:
     `sudo systemctl restart rusty-photon-<service>` on Linux,
     `Restart-Service rusty-photon-<service>` on Windows, `brew services
     restart rusty-photon-<service>` on macOS, since the unit, the Windows
     service and the Homebrew service all carry the one name. The pages
     name the saved fields still waiting, with the same instructions, until
     the restart runs them;
   - when `server.port` or `server.bind_address` changed, the URL the page
     will have: the host the browser used, the new port;
   - when a device page disabled its device, that the device's page is no
     longer served, and the wait reopens the server page instead, which
     lists the device and names the file as the way to enable it again.
     Both are read off the configuration the reloaded service runs, so a
     device whose `enabled` a flag keeps on stays on its page whatever the
     file says.

One save at a time per service, from the file read to the persist: every
page saves to the one file, and two saves interleaving would have the later
one persist its stale copy of the other's section.

Nothing a `GET` does changes state.

## A driver's own part

A driver whose configuration the walker cannot render, or whose devices
its configuration alone cannot name, adds its own part to its pages: a
`DriverPages` the handler is built with (`SetupPages::with_driver_pages`).
The camera drivers are the first: their `usb_devices` list is an array of
objects, which the walker names as file-only, and the devices a camera
server serves follow the USB placement its last start or reload found, not
the file (the driver-setup-pages plan's D11). `zwo-focuser`'s devices are
the EAFs its SDK enumerates, which the file only renames through a
`devices` map of overrides, so its part (`rusty-photon-driver`'s
`overrides` module) is a page per EAF that edits that EAF's override. A
`DriverPages` says:

- **which top-level keys its sections edit.** The walker leaves them off
  every page, so no field is edited in two places, and no file-only note
  stands for a key a section edits.
- **which devices the server serves**, when the driver knows better than
  its configuration does: the server page's device list and the device
  pages follow these instead of `device_sections`. A device with no section
  of the file's own has an empty pointer, and its page is the driver's
  sections alone: what the driver knows about it, and whatever of it the
  driver lets a page edit. A page whose every part submits nothing — a
  page that only shows — has no form and no Save button.
- **its sections on a page**, asked for on every request, so a section can
  read the host as it is now (the camera drivers' USB scan), and told
  whether the request saves, so work only a save needs is skipped on a page
  that is only shown.

A section draws its markup inside the page's form — from the file as the
page shows it, or, re-rendering a save, from the submission and its errors
— and applies a submission onto the file the save is building, naming any
error by a control it draws. It is drawn knowing the host the browser
reached the page by, so a link it draws to another server the configuration
names by a loopback address carries that host instead
(`origin::link_for`: `127.0.0.1`, `::1` and `localhost` are replaced, the
scheme, port and path kept; any other host is left as written). A section
whose configuration is an array of entries draws each entry's fields with
`FieldModel::from_node_under`, which walks the entry's schema under its
place in the file (`equipment.cameras.0`), so each field is named as the
service's validation names its errors and labelled by its own key. `rp`'s
roster is the first ([rp.md § Setup Page](../services/rp.md#setup-page)). An error a section draws beside its control is
not repeated above the form; any other is named there with its path, as
for a field the page does not show.

### Confirmation

A section can ask that a save be confirmed before it is written: one that
moves a camera to another device number, which every client bound to the
old number must follow (D11). After the save has passed validation — and
only when it changes something, so fires a reload, since a save that
changes nothing has nothing to confirm — each section on the page is
shown the file before and after it; one that wants
the change confirmed answers what it would change and a token naming
exactly that. When the submission's `confirm` field does not carry the
tokens the same submission produces now, nothing is written: the page
answers `409 Conflict` with the form as submitted, what the save would
change above it, and a hidden `confirm` field carrying the tokens, so the
operator who saves that page again confirms what it showed. A
confirmation covers the change it showed and no other: a submission
whose changes differ, or a file that moved on in between, asks again.

A page never carries a password back, so a confirmation page asks again,
beside the field, for a password the submission typed: saved without it,
the stored one stays (and with none stored, the save is refused as any
username without a password is). A submission that carries the current
tokens keeps its confirmation through a rejection, so fixing the field —
that password, say — is one save, not another round of the
confirmation.
## Credentials

The server page's **Security** section edits `server.auth` through three
inputs: username, password, repeat. The page never carries the stored hash.

- A username with a password (and its repeat equal) hashes the password
  with Argon2id (`rp-auth`, on the blocking pool) and persists
  `server.auth` with the hash — once every other value has passed: a
  rejected save hashes nothing, and its re-render says what the file
  holds.
- A username with a blank password keeps the stored hash; with no hash
  stored, the save is refused with the password field named — turning auth
  on needs a password.
- A password whose repeat differs is refused with the repeat field named.
- A blank username with blank password fields turns auth off:
  `server.auth` is absent from the file. A blank username with a typed
  password is refused with the username field named — a contradiction,
  not a request.

A secret leaf the driver names in `secret_pointers` that is not the
server-auth hash renders as an empty password input whose blank submission
means "unchanged", and never carries its stored value.

**A credential the service presents to another server** is bound to the
settings that decide which server receives it — its address, and the CA that
verifies the server there: `SetupPages::with_bound_credential(setting,
credential)`, two JSON pointers, one call per setting. A page anyone who
reaches the service can save must not send a stored password to whatever
server is typed on it, so a save that changes a bound setting and leaves the
credential block as the file stores it asks first:

- It answers `409` ([Confirmation](#confirmation)), saying it would remove
  the block and which setting changed (`#confirm li.unbind`). Beside the
  Save button is a password control for the block (`keep-credential:` and
  the block's dotted path), when the driver names a secret directly under
  it in `secret_pointers` and the running service has its own credential
  set (the block at `with_auth_pointer`'s pointer). A page anyone can reach
  must not answer whether a guess matches a stored password, so behind no
  login the confirmation says it cannot keep the block, and a keep control
  sent anyway is ignored.
- Saved with the stored password typed there (compared in constant time),
  the block is kept for the new server. A typed password that does not match
  is refused beside the control, the file untouched. A page never carries a
  password back, so a refusal for another field asks for it again beside the
  control rather than let the next save remove the block unawares.
- Saved with the control blank, the block is removed, and the saved page
  names it and the settings that changed (`#saved-unbound`) and says to set
  it again in the file. That page waits for no reload: on a service that
  reloads it stays on screen, with a link back, instead of reopening the
  setup page, whose file-only note would not say the block is gone.

A setting is compared as the service would run it — parsed and normalized,
so a default the file leaves out, written back by the page, or a blank
optional field submitted as `null` is no change — and a block the save itself
rewrote is kept, as the operator's for the new server. The BFF
binds `rp.auth` to `rp.base_url` and `rp.ca_cert_path`
([ui-htmx.md](../services/ui-htmx.md) § Setup page), and planetarium-bridge
binds `rp.service_auth` to `rp.mcp_server_url` and `rp.ca_cert`
([planetarium-bridge.md](../services/planetarium-bridge.md) § Setup pages).

## Cross-site protection

Browsers resend cached Basic credentials on any request to the origin, so
a form `POST` behind Basic Auth could be submitted by a page on another
site. Every `POST` is accepted only when the request says it came from the
page itself:

- `Sec-Fetch-Site` is `same-origin` or `none`; or
- the request carries no `Sec-Fetch-Site` and its `Origin` header's host
  and port are the request's own `Host`.

A `POST` with neither header, or with either naming another site, is
refused with `403` and a sentence saying why. No token, no cookie, no
state. A client that is not a browser sends an `Origin` equal to the
server's `Host` to be accepted.

## Reload

The reload is `rusty-photon-service-lifecycle`'s `ReloadSignal`, fired
from a detached task after `RELOAD_AFTER_RESPONSE_DELAY` so the response
flushes before the server that served it is torn down. The constant lives
here; `rusty-photon-driver`'s config-action dispatch re-exports it until
the actions retire.

## What is not here

- Nothing Alpaca: the glue from the crate's `SetupRequest` and a device's
  `setup_request` hook to the handler is `rusty-photon-driver`'s.
- No navigation between services of its own: a page links to its own
  server page and its own device pages. A service's own part may link
  another server, through `origin::link_for` (`rp`'s roster links each
  entry's driver).

## Testing

Unit tests in the crate pin the walker (every row of the table above, the
sorted order, `$ref` resolution, the tagged-enum detection against
`schemars`'s own output), the coercion rules, the renderer (a control per
kind, the pinned state, a file-only note, the Security section), the
origin check, and the handler against a fake driver (the overlay onto one
section, a section the file leaves out shown with its defaults and saved,
left alone keeping its own default, and started with the fields the page
does not send,
the skipped pinned field — inside a tagged enum too — the
rejected save leaving the file untouched, a `validate` error re-pointed at
a variant's control, the persisted file, the reload fired, the credential
rules, one save waiting for another), and a driver's own part (its keys
left off every page, the device list it gives, a page it draws alone with
no form when nothing on it submits, its section's apply and errors, and
the confirmation: the `409` with its token, the save the token confirms,
and a token that no longer matches asking again), and a bound credential
(the `409` before its removal, the removal with its control blank, the
block kept by its password typed again, a wrong one refused beside the
control, a refusal for another field asking for it again, no control behind
no login and one sent anyway ignored, and a saved page naming a removal that
waits for no reload).

Each service pins its own contract in a `setup_pages.feature` driven by
the shared steps in `bdd-infra`'s `setup_pages` module
([testing.md §5.1](../skills/testing.md#51-shared-infrastructure-bdd-infra-crate));
`dsd-fp2` is the reference. Every page carries its handler's generation in
a `<meta name="setup-generation">`; a reload builds a new handler, so the
step that waits for a saved value after the reload accepts only a page
whose generation differs from the one before the save — the old listener,
which already shows the file's new value, cannot pass for the rebuilt
one.

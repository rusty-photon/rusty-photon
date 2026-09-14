# Device Claims + the PHD2 Guide-Camera Facade

## Goal

Two problems that look unrelated share one root: **the guide camera is the
only device in the rig that no Alpaca driver may own.**

1. **Every SDK-backed driver claims every device its SDK enumerates.**
   `qhy-camera`, `zwo-camera` and `svbony-camera` register each camera the
   vendor SDK reports. There is no way to say *"this one belongs to PHD2."*
   For QHY it is worse than an ownership question: `Sdk::new()`
   ([`crates/qhyccd-rs/src/sdk.rs`](../../crates/qhyccd-rs/src/sdk.rs)) runs
   `cfw_probe` — `OpenQHYCCD` → `InitQHYCCD` → `IsQHYCCDCFWPlugged` ×3 →
   `CloseQHYCCD` — against **every** camera the scan returns, before any
   config is consulted. `InitQHYCCD` is a full device configuration, not a
   read. Run against a camera PHD2 is streaming, it is the disruptive call.
   The probe runs at service start, again on config reload, and inside
   `qhy-camera doctor`.

2. **The guide camera is outside Alpaca, so rp cannot capture through it.**
   PHD2 owns it at the SDK level, which is why
   [`rp.md` § Guide-train sweep](../services/rp.md) makes guide-train
   autofocus a special case: it moves the focuser and reads PHD2's per-frame
   HFD instead of capturing. That variant requires an *active guide loop*,
   which is exactly the wrong precondition for "focus the guide camera
   before the night starts."

The outcome of this plan:

- **Part A — claims.** Each SDK-backed driver gains a `claims` config block
  naming which enumerated devices it registers, keyed on a stable identity,
  applied **before** any device-touching probe. A driver never opens a
  device it does not own.
- **Part B — the PHD2 camera facade.** `phd2-guider` additionally serves an
  ASCOM Alpaca **Camera** device backed by PHD2's own frame capture, so the
  guide camera appears in rp's roster like any other camera and the ordinary
  `auto_focus` capture sweep works on the guiding train.

Part A makes Part B safe (the guide camera is explicitly excluded from the
vendor driver instead of accidentally skipped), and Part B makes Part A
worth doing (the excluded camera is still reachable, through PHD2).

## Implementation Status

| Phase | Description | Status | Branch / PR |
|-------|-------------|--------|-------------|
| C0 | This plan | Not started | |
| C1 | `claims` schema + `svbony-camera` (identity is free at enumeration — proves the schema) | Not started | |
| C2 | `claims` in `zwo-camera` (passive open to read the serial) | Not started | |
| C3 | `claims` in `qhy-camera` + `qhyccd-rs` enumerate/probe split — the actual defect fix | Not started | |
| C4 | Stable `device_number` assignment, `doctor` claim-awareness, `config.schema`/`config.apply` exposure | Not started | |
| C5 | `phd2-guider` Alpaca Camera facade (design doc → BDD → code), incl. the `save_image` wire-format fix | Not started | |
| C6 | rp wiring: guide camera as a train-terminal camera, capture-sweep AF on the guiding train, doc updates | Not started | |
| C7 | `ui-htmx` claims editing | Deferred | |

Order: C1 → C2 → C3 are independent of Part B and each land on their own.
C4 folds in once two drivers carry `claims`. C5 needs nothing from Part A
but is only *operationally* safe after C3. C6 needs C5.

Each phase follows
[development-workflow.md](../skills/development-workflow.md): design-doc
update first (`qhy-camera.md` / `zwo-camera.md` / `svbony-camera.md` /
`phd2-guider.md` / `rp.md`), BDD second, implementation third.

---

## Part A — Device claims

### D1. What identity is actually stable

Surveyed against the three vendored SDK crates rather than assumed:

| SDK | Identity | Available without opening the device? | Notes |
|-----|----------|----------------------------------------|-------|
| QHYCCD | `GetQHYCCDId(index)` → `QHY268M-<serial>` | **Yes** | Model and serial in one string. The id is already the crate's `Camera::id()`. |
| ZWO ASI | `ASIGetSerialNumber` → 16-hex string | **No** — needs `ASIOpenCamera` | `zwo-rs` already has `open_uninitialised()` for exactly this: `ASIOpenCamera` without `ASIInitCamera`, documented as not affecting a capturing camera, written for tenet *no actuation on connect*. Old models may have no serial. |
| SVBony | `CameraSN` inside the enumeration struct | **Yes** | Best case — serial arrives with the listing. |

**USB path is rejected as the key.** No vendor SDK exposes it; deriving it
means sysfs/udev on Linux and SetupAPI on Windows, which is neither portable
nor available through the SDK handle we actually hold. It also has the wrong
semantics: a USB path identifies *the port*, so moving a cable silently
re-points the claim. A serial identifies *the camera*, and the claim should
follow the camera.

**Decision: the SDK serial is the key.** Name is a human-readable
disambiguator, never a key. Index is an escape hatch for serial-less
hardware, explicitly marked as fragile in the config docs.

This is not a new vocabulary: all three services already key their
`devices` override map by SDK serial, and all three already derive the
ASCOM `UniqueID` as `VENDOR:{name}:{serial}`. `claims` reuses the key the
operator already types.

### D2. Config shape

Added to each camera service's `Config`, next to `devices`:

```json
{
  "claims": { "mode": "all" },

  "claims": { "mode": "include", "serials": ["b4a1f2ab3c4d5e6f"] },

  "claims": { "mode": "exclude", "serials": ["QHY5III715C-2a9c04f1e7b3"] },

  "claims": { "mode": "include", "indices": [0] }
}
```

- `mode: "all"` is the default, so every existing config keeps working
  unchanged.
- `include` and `exclude` take `serials` (preferred) and/or `indices`
  (escape hatch). Both empty under `include` registers nothing, which is
  legal and logged loudly — it is how an operator temporarily hands a whole
  SDK to another application.
- `deny_unknown_fields`, like the rest of the config tree.

The guide-camera case is one line in `qhy-camera.json`:

```json
"claims": { "mode": "exclude", "serials": ["QHY5III715C-2a9c04f1e7b3"] }
```

### D3. Behaviour

1. **Claims are applied before any device-touching call.** This is the whole
   point and the one rule that cannot be relaxed per driver. Enumeration
   yields identities; the filter runs; only survivors are probed, opened, or
   initialised.
2. **A claimed serial that is not present** registers nothing, logs
   `warn!`, and produces a *soft* `doctor` finding ("claimed device not
   found"). It is never a startup failure: a cold camera or a powered-down
   hub is a normal Tuesday, and a driver that refuses to start because one
   of three cameras is absent is worse than one that starts with two.
3. **An enumerated device whose identity cannot be read** cannot be claimed
   by serial. Under `include` it is skipped (we cannot prove it is ours);
   under `all`/`exclude` it is registered with a warning — preserving
   today's behaviour exactly.
4. **`doctor` becomes claim-aware.** It still *lists* everything the SDK
   enumerates (that is its job — telling the operator what is plugged in),
   but marks each device claimed/unclaimed and **does not probe unclaimed
   devices**. Today `qhy-camera doctor` runs the full open+init probe over
   every camera; run mid-session that reaches into PHD2's guide camera.
5. **`device_number` stability.** Alpaca device numbers are assigned by
   enumeration order today, so unplugging one camera renumbers the others
   and silently re-points every `cameras[].device_number` in rp's config.
   Claims make this worse (the claimed set changes shape). Assign device
   numbers by **sorted claimed serial** instead, so a device number is a
   function of *which cameras are claimed*, not *which are currently
   plugged in or what order the SDK scanned them*. This is a correctness
   fix worth doing even without claims; it is a breaking change for any
   multi-camera config and needs a release note.

### D4. Per-driver work

**`svbony-camera` (C1)** — filter the `CameraInfo` list before registering.
No SDK call changes. Proves the schema, the doctor integration and the BDD
pattern on the easy case.

**`zwo-camera` (C2)** — unavoidable wrinkle: the serial is only readable
after `ASIOpenCamera`, so a serial-keyed claim still requires the passive
open of each enumerated camera. That is the already-blessed
`open_uninitialised()` path (`ASIOpenCamera` only — no `ASIInitCamera`,
no control writes, closed on drop), which is why ZWO cohabitation is
tolerable today. Document the honest limit: **ZWO cannot be filtered
fully-passively by serial**; operators who want zero touching must claim by
`indices`. Do not paper over this.

**`qhy-camera` (C3)** — the defect fix. Split the vendored crate's
`Sdk::new()` into two stages:

```rust
// stage 1: identities only — ScanQHYCCD + GetQHYCCDId, no device is opened
let ids: Vec<String> = Sdk::enumerate_ids()?;

// stage 2: probe (open → init → CFW → close) only the claimed ids
let sdk = Sdk::open_claimed(&claimed_ids)?;
```

`qhyccd-rs` stays generic — it takes the claimed set, it does not learn
about rusty-photon config. The service applies `claims` between the two
stages. This alone removes the "our scan disturbs PHD2's guide camera"
failure mode, claims or no claims, at service start, on reload, and in
`doctor`.

### D5. Out of scope for Part A

- Filter wheels, focusers and rotators. The pattern generalises, but the
  motivating conflict is cameras and `zwo-focuser`/`qhy-focuser` have no
  competing consumer today. Widen when a second consumer appears.
- Hot-plug. Claims are resolved at enumeration (start / reload), not
  watched. A camera plugged in mid-session is picked up by a reload, as
  today.
- `ui-htmx` editing (C7).

---

## Part B — PHD2 as an Alpaca Camera

### D6. What PHD2 can actually deliver

Verified against PHD2's EventMonitoring wiki, not assumed:

| RPC | Delivers | Constraint |
|-----|----------|------------|
| `capture_single_frame{exposure, subframe}` | `integer(0)` — an acknowledgement, no pixels | **"guiding and looping must be stopped first"** |
| `save_image` | `{"filename": "<full path to FITS>"}` on **PHD2's** host; *"the client should remove the file when done with it"* | Whole frame, but only as a path |
| `get_star_image{size}` | `{frame, width, height, star_pos, pixels}`, base64 16-bit row-major | Only a ≥15 px cutout, and errors unless a star is selected |

So the facade is `set_exposure` + `capture_single_frame` + poll + `save_image`
+ decode FITS. There is no full-frame-over-the-wire path, and no "frame
done" event — completion is observed by the app state returning to
`Stopped`.

**Prerequisite defect:** `Phd2Client::save_image`
([`services/phd2-guider/src/client.rs`](../../services/phd2-guider/src/client.rs))
parses the result as a bare string, but PHD2 returns the object above. It
fails against real PHD2 every time and passes CI only because
`mock_phd2.rs` returns the same wrong shape. Fix the client, the mock
(which must also serve a real small FITS for the facade's tests) and the
`save_image` row in `phd2-guider.md` as the first commit of C5.

### D7. Shape of the facade

- **Where.** Inside the `phd2-guider` binary, as a second server: add
  `ascom-alpaca = { features = ["server", "camera"] }` alongside the
  existing axum service. The Alpaca server owns its own routing, discovery
  and management API, so it gets **its own port — proposed 11128** (free;
  sits in the driver block 11119–11127 rather than next to the rp-managed
  services, because what it serves *is* a driver-shaped device). It reuses
  the shared `AlpacaServerConfig` block like every other driver.
- **Opt-in.** `camera.enabled` defaults to `false`. An unrequested second
  Camera device in the roster is confusing, and enabling it costs PHD2
  round trips at startup.
- **Exposure.** `StartExposure(duration, light)` → `set_exposure(ms)` then
  `capture_single_frame`. `ImageReady` stays `false` until the app state
  returns to `Stopped`, then `save_image` → read → **delete** the file →
  decode to the `ImageArray` cache.
- **Capability surface.** `CanAbortExposure`/`CanStopExposure` `false`
  (PHD2 offers no cancel for a single frame), no cooler control, no
  gain/offset (PHD2 owns those through its equipment profile), bin 1 only.
  `CameraXSize`/`CameraYSize` from `get_camera_frame_size`. `MaxADU`
  65535. Passing ConformU with a surface this narrow is a real work item,
  not a footnote — budget for it in C5 the way `svbony-camera` did.
- **`PixelSizeX`/`PixelSizeY` come from config.** PHD2 exposes
  `get_pixel_scale` (arcsec/px), which is pixel size *divided by* focal
  length — not recoverable without the focal length, and only valid after
  calibration. rp reads `PixelSizeX` off the terminal camera for train
  optics, so the facade must be told: `camera.pixel_size_um`, required when
  `camera.enabled` is true, rejected at load if non-positive or non-finite.
  Document it as operator-entered from the guide camera's datasheet.
- **Same-host assumption (v1).** `save_image` writes on PHD2's host; the
  facade reads it back from the filesystem. `phd2-guider serve` colocated
  with PHD2 is the deployment we already document. A non-colocated setup
  fails with a structured `phd2_image_unreadable` error naming the path —
  never a silent empty frame. A shared/`image_dir` mapping is deferred.

### D8. The exclusivity contract (the part that must not be got wrong)

Guiding and single-frame capture are mutually exclusive *in PHD2*, so the
facade must make that explicit rather than resolve it:

- `StartExposure` while PHD2 is guiding or looping → an Alpaca
  `InvalidOperationException` naming the app state. **It must never stop
  the guide loop to service a capture.** Stopping guiding is an explicit
  operator/rp action (`POST /api/v1/guiding/stop`), never a side effect.
- `Connected = true` on the facade must not expose, not stop guiding, not
  touch the PHD2 profile — workspace tenet 3, *no actuation on connect*.
  Connecting only establishes the JSON-RPC session the guider service
  already holds.
- Conversely, `POST /api/v1/guiding/start` while a facade exposure is in
  flight waits for the frame or fails; it does not race `capture_single_frame`.
- Both paths share one PHD2 connection, so serialise them in `GuiderOps`
  behind the same lock, and report the loser with a structured error rather
  than queueing silently.

### D9. What this changes in rp (C6)

The guide camera becomes an ordinary `cameras[]` entry — `alpaca_url`
pointing at the facade, `device_number: 0` — and therefore a legal terminal
camera of the guiding train. Then:

- `auto_focus` addressed at the guiding train can run the **ordinary
  capture sweep** (`move_focuser` + `capture` + `measure_basic`), with the
  precondition *guiding stopped*.
- The existing **PHD2-metric sweep is kept, not replaced.** The two have
  opposite preconditions — the metric sweep needs an active guide loop, the
  capture sweep needs no guide loop — so they cover different moments:
  metric for mid-session refocus while guiding, capture for start-of-night
  focusing. `auto_focus` picks by current guiding state, or by an explicit
  parameter; that choice is C6's design-doc decision.
- `rp.md`'s flat statements that the guide camera "is never captured
  through — PHD2 may own it at the SDK level" (three places) become
  conditional on whether the facade is configured. The same sentence in
  [`optical-trains.md`](optical-trains.md) needs the same treatment.
- The Guide Focus Watch keeps reading `GuideStep` HFD; nothing there
  changes.

### D10. Why not the alternatives

- **`get_star_image` as the image source** — needs a selected star and
  gives a ≤32 px cutout. Fine for a star-profile display, useless for a
  focus sweep that must measure several stars across the field, and it
  cannot work before a star is selected (i.e. exactly when you want to
  focus).
- **A second Alpaca driver owning the guide camera, PHD2 reading through
  it** — PHD2 has no Alpaca backend on Linux/macOS at all, and on Windows
  only through a manually registered COM dynamic client. Not a path.
- **Time-slicing the guide camera between `qhy-camera` and PHD2** — the
  vendor SDK hands out a device once; "release it between exposures" means
  open/init churn against a camera in a control loop. This is the failure
  mode Part A exists to prevent, not a design.

---

## Open questions

1. **Facade port.** 11128 (driver block) as proposed, or 11132 (next to the
   rp-managed services 11130/11131)? The facade is served by an rp-managed
   service but *is* a device.
2. **Pixel size.** Config-required as in D7, or read once from a FITS
   header if PHD2's guide camera driver writes `XPIXSZ`? The header route
   removes an operator step but is vendor-dependent; D7 takes the explicit
   route.
3. **`claims` default.** `all` forever (back-compatible, D2), or warn after
   a release or two so multi-camera rigs are pushed toward explicit
   ownership?
4. **`device_number` reassignment (D3.5).** Breaking for existing
   multi-camera configs. Land it inside C4 with a release note, or defer to
   a major version?

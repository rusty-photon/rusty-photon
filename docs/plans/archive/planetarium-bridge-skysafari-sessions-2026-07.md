# planetarium-bridge: the SkySafari sessions, July 2026

A record, moved whole from `docs/services/planetarium-bridge.md` on
2026-11-11: what two sessions of SkySafari Pro 8.0.3 driving a throwaway
Alpaca spike showed — the verbs it sends, the epoch on the wire, the poll
cadence, the horizon gate on GoTo and the reported-position wedge — and
the design consequences drawn from them. "This document" in the text is
[planetarium-bridge.md](../../services/planetarium-bridge.md).

## P3b horizon experiment — closed 2026-07-30

Run against the spike with a second device (identical SkySafari
build); full findings in the
[P3b appendix](#appendix-p3b-horizon-experiment-findings-2026-07-30).
The three questions resolved: (1) an object-GoTo to a below-horizon
target is refused under **every** horizon display setting — the gate
is unconditional; (2) the wedge did **not** reproduce on the second
device under any setting — device/state-dependent, cause
unattributed, floor policy retained; (3) coordinate-entry GoTo **is**
horizon-gated below 0° — P3a's "ungated" observation had only ever
reached an above-horizon point. Net consequence: GoTo cannot express
below-horizon planning at all, which drove Decision 2's second
amendment (§ [Align is the import gesture](../../services/planetarium-bridge.md#align-sync-is-the-import-gesture)).

---

## Appendix: P3a verification-spike findings (2026-07-29)

Session: 2026-07-29 (UTC), **SkySafari Pro 8.0.3 (build 1205)** on iPad
driving the spike over Wi-Fi, ~20 minutes of traffic; the JSONL wire
log is the raw evidence (kept off-repo with the operator). Findings are
from this one client/version; the plan's SkySafari floor is v7
(Decision 1), not separately tested. The spike crate
`spikes/planetarium-bridge-p3a` (throwaway, sanctioned per Decision 8 /
ADR-005) stayed runnable through the P3b experiment above and was
deleted when the bridge implementation landed.

### The P3a questions — answered

| # | Question | Verdict | Finding |
|---|----------|---------|---------|
| 1 | Discovery and connection lifecycle | Answered | Alpaca UDP discovery is **subnet-local** (broadcast; does not cross routed segments — the iPad and spike host were on different subnets and no discovery datagram ever arrived). Manual IP:port entry works across subnets, confirming the plan's documented-default posture. Lifecycle detail below. |
| 2 | Which slew/sync verbs | Answered | GoTo = **`SlewToCoordinatesAsync`, exclusively** (never the blocking variant, never `SetTarget*`+`SlewToTarget`). Align = **`SyncToCoordinates`** (never `SyncToTarget`). The verbs are cleanly distinct — Decision 2's sync-is-not-intent rule is safe. Numbers arrive in **scientific notation** (`RightAscension=1.341988e+01`); parsers must accept it (our `ascom-alpaca` fork does). |
| 3 | J2000 honored, or JNow? | Answered | **J2000 honored.** Five object GoTos (Spica, M 92, M 13, Draco Dwarf, HD 142596) all matched the target's J2000 position to arcseconds; the automated probe verdict on M 13 read 0.04′ (J2000 frame) vs 12.13′ (JNow frame). `EquatorialSystem` is read **once, at connect** — changing the declaration requires a reconnect. No epoch setting exists anywhere in SkySafari's scope UI, so this reads as client behavior (honor the device declaration), not per-install configuration; the `assume_epoch` override stays as a safety valve. |
| 4 | Position-report cadence | Answered | A steady **1 Hz cycle** of `Slewing` → `Tracking` → `RightAscension` → `Declination` (four GETs ~10 ms apart, every ~1.0 s), identical while idle and while slewing. The spike's 3 s simulated convergence satisfied it: SkySafari showed the slew as arrived when `Slewing` flipped false. No connection timeouts observed. |
| 5 | **Go/no-go: arbitrary-point GoTo** | **GO, with caveats** | Tapping empty sky offers **no** GoTo — an object must be selected. But **coordinate entry (Search → coordinates) exists and GoTos arbitrary points**. Caveat: the entry form has **no epoch choice and interprets input as equinox-of-date (JNow)**, converting to J2000 on the wire — proven by a round-number probe: entered 14h00m00s / −40°00′00″ arrived as 13h58m23s / −39°52′11″, which is exactly the J2000 equivalent of the entered values read as JNow (the RA −1m37s / Dec +7.7′ shifts match 26.6 years of precession at that position to the second). A second practical path: SkySafari's selectable catalog reaches faint HD/Tycho stars and obscure PGC galaxies — a star within arcminutes of any intended frame center almost always exists and GoTos with exact J2000 coordinates. |

### Connection lifecycle detail

- **Preset editor probe** (before any connect): `apiversions` →
  `configureddevices` → `apiversions` → `alignmentmode` →
  `canslewasync` → `canslewaltazasync`, under throwaway `ClientID`s.
- **Connect**: `PUT Connected=true`, then a property battery —
  `EquatorialSystem` (once), site latitude/longitude, `UTCDate`, and a
  capability sweep (`CanSync`, `CanSyncAltAz`, `CanSetTracking`,
  `CanPark`, `CanMoveAxis`, `CanSlewAsync`, `CanSlewAltAzAsync`) — then
  the 1 Hz poll loop.
- **SkySafari pushes site and time to the device**: `PUT SiteLatitude` /
  `PUT SiteLongitude` (its own GPS-derived values) and `PUT UTCDate`.
  The spike accepted the site writes; `SetUTCDate` answered
  `NOT_IMPLEMENTED` (1024) — SkySafari retried once ~23 s later and
  carried on unaffected.
- **Disconnect sends nothing**: polling simply stops — no
  `Connected=false` was observed. The bridge must not depend on an
  explicit disconnect signal (idle-timeout thinking only).
- `ClientID` is **not stable** across app contexts: the main scope
  panel used one value, other gestures another. Treat it as
  diagnostic, not identity.
- SkySafari's **Center** button is display-only (no wire traffic) —
  only **GoTo** and **Align** reach the device.

### Below-horizon behavior

- **GoTo is horizon-gated client-side.** A below-horizon target's GoTo
  is refused by SkySafari — sometimes silently (nothing on the wire, no
  dialog), sometimes with a warning dialog. Couch-planning an object
  that has not yet risen therefore **cannot** be imported by object-GoTo
  at that moment; the operator must import while the target is up, or
  use coordinate entry (not horizon-gated in the observed case, which
  reached a 17°-altitude point).
- **Align/Sync was NOT horizon-gated** — a sync to below-horizon M 31
  went through (moot for the real bridge: sync is now rejected).
- **Wedge hazard**: after that sync put the virtual scope's reported
  position below the horizon, SkySafari refused *every* subsequent GoTo
  ("stuck") until the reported position returned above the horizon
  (fixed server-side with a corrective sync). This drove the
  [reported-position policy](../../services/planetarium-bridge.md#reported-position-policy-the-altitude-floor):
  the hazard exists with no sync at all, since a tracked position
  imported at dusk sets hours later.

### Design implications carried into this document

1. Manual IP:port is the primary connection story → § Discovery.
2. The GoTo/Sync verb split is exactly as Decision 2 assumed; sync
   carries zero intent → sync was rejected at design time (chosen over
   the plan's accept-and-ignore, 2026-07-29; **reversed by P3b** —
   § Align is the import gesture).
3. The wire is J2000 end-to-end for this client → `assume_epoch`
   default `"j2000"`, kept as cheap insurance.
4. Site/UTC pushes and scientific-notation floats → § Site and time
   writes.
5. Reported position must never linger below the horizon → § the
   altitude floor.
6. SkySafari's coordinate-entry box is JNow (no epoch choice) —
   operator docs must warn that J2000 coordinates typed there land
   ~20′ off; framing via a nearby faint catalog star avoids the
   pitfall entirely.
7. No explicit disconnect arrives → no bridge state may depend on one.

---

## Appendix: P3b horizon-experiment findings (2026-07-30)

Session: 2026-07-30 (UTC), **SkySafari Pro 8.0.3 on a phone — the
same app build as P3a's iPad** — driving the same spike over Wi-Fi
(manual IP:port across routed subnets, as before); the JSONL wire log
is the raw evidence (kept off-repo with the operator). The build
identity matters: every P3a/P3b behavioral difference below is
device- or state-dependent, not a version difference.

### The P3b questions — answered

| # | Question | Verdict | Finding |
|---|----------|---------|---------|
| 1 | Object-GoTo below horizon still refused with horizon display off/transparent? | **Refused, always** | NGC 253 (≈ −17° to −20° alt) GoTo refused with the horizon off, transparent, and on. Nothing reaches the wire (client-side refusal). The gate is unconditional. |
| 2 | Does the reported-position wedge still occur? | **Did not reproduce** | After a below-horizon Align (reported position ≈ −20°), an object GoTo to the Moon (≈ 24° up) went through — under every horizon display setting. The P3a iPad wedged on the identical build. Device/state-dependent; unattributed. The altitude floor stays as defense. |
| 3 | Coordinate-entry GoTo still ungated below 0°? | **Gated** | RA 6h / Dec +20° (≈ −37° alt) refused with "command failure cannot go to". Overturns P3a's impression — its coordinate-entry probe had only ever reached a +17°-altitude point, so the below-0° case was never actually exercised until now. |

### Additional findings

- **Align is never horizon-gated.** Three below-horizon
  `SyncToCoordinates` for NGC 253 arrived J2000-exact
  (0.79253h / −25.2875°), matching the catalog centroid to the
  arcsecond each time.
- **Align is object-only**: SkySafari offers no Align on an entered
  coordinate point, so arbitrary-point import rides the
  faint-star-adjacent anchor gesture at any altitude.
- **The faint-star anchor gesture works end-to-end.** An Align on a
  mag 8.15 field star near NGC 6633 arrived 0.12″ from the packed
  catalog's Tycho-2-derived position for **HD 170881**; run through
  `rp_catalog::nearest()` (post-#767 catalog, 10′/2′ tolerances) the
  point names as `HD 170881` dead-center, while NGC 6633 — 62′ away,
  outside its DSO cone — correctly does not outrank the anchor. A
  second Align on a mag 9.27 star with nothing else in frame resolved
  identically (**HD 172011**, 0.12″): the anchor works in empty
  fields too, and SkySafari's selectable depth comfortably reaches
  the HD layer's magnitude range.
- **Center is display-only** (crosshair motion, zero wire traffic) —
  re-confirmed; it is the visual-confirmation half of the
  Center → Align workflow.
- The coordinate-entry form exposed **no minus sign** for declination
  in this session — the below-horizon probe used a positive-dec
  anti-meridian point instead. (P3a *did* enter −40°, so the sign
  control's availability varies with device or context.)
- Two Moon GoTos 16 minutes apart arrived with coordinates drifted
  ~8′ — solar-system GoTos carry live positions.

### Design implications carried into this document

1. GoTo cannot import below-horizon targets under any client setting
   → the import gesture is **Align** (Decision 2's second amendment,
   § Align is the import gesture); slews are simulated motion only.
2. The wedge is real (P3a) but not reliably reproducible (P3b) → the
   altitude floor stays, default `10.0`, `null` documented as safe
   for wedge-free clients.

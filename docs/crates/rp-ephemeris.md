# `rp-ephemeris` Crate Design

Astronomical math for Rusty Photon: a single trait that returns
positions, times, and twilight windows, plus an in-process
implementation that wraps the BSD-licensed ERFA library.

This is a workspace library, not a service. The `rp` orchestrator
consumes it directly, as do `polar-align` and `planetarium-bridge`.
See [`docs/services/rp.md`](../services/rp.md) for how the planner and
MCP tools project the trait onto the external surface; this doc covers
the crate's own design.

## Scope

- Sidereal time, alt/az, transit, rise/set, meridian-flip, sun/moon
  positions, twilight windows, moon separation.
- Pure functions: no I/O, no file parsing, no global mutable state.
- Trait surface contains zero `unsafe` and no FFI types — the only
  `unsafe` lives below the `erfars` boundary inside the ERFA C
  bindings.
- No catalog resolution, no plate-solving, no policy. Those live in
  sibling crates / services.

## The `Ephemeris` Trait

The single seam between the math layer and everything that consumes
positions:

```text
sidereal_time(site, time)                              -> LocalSiderealTime
alt_az(site, target_icrs, time)                        -> Result<AltAz, EphemerisError>
transit(site, target_icrs, date)                       -> Option<DateTime<Utc>>
rise_set(site, target_icrs, date, min_alt_deg)         -> Option<RiseSet>
meridian_flip(site, target_icrs, time, side_of_pier)   -> Option<Duration>
sun_position(site, time)                               -> SunInfo
twilight(site, date, kind)                             -> TwilightWindow
moon_position(site, time)                              -> MoonInfo
moon_separation(target_icrs, time)                     -> f64 (degrees)
```

Methods borrow the `Site`, take the target and the time by value
(`IcrsCoord` and `DateTime<Utc>` are `Copy`), and return owned values.
Implementations may not retain mutable state across calls — `&self` is
reserved for caching only. `meridian_flip` takes the side of pier but
does not consult it: it returns the time to the next meridian crossing.

`Site` is constructed via `Site::new(latitude_degrees,
longitude_degrees)`, which validates range and resolves the IANA
timezone once via `tzf-rs`. The timezone is stored as a `&'static
str` borrowed from a process-static finder. A site has no elevation:
the transforms run at sea level, which
[rp.md § Site Configuration](../services/rp.md#site-configuration)
explains is enough. `Site::night_date(at)` names the observing night an
instant belongs to — the night rolls at local noon, in the site's
timezone under `chrono-tz`'s DST rules
([rp-targets.md § Progress derivation](rp-targets.md#progress-derivation-the-actuals)).

## `ErfarsEphemeris` Implementation

The shipped impl wraps the [`erfars`](https://crates.io/crates/erfars)
crate (Rust FFI for ERFA — the BSD-licensed clean-room derivative
of IAU SOFA used by Astropy and LSST/Rubin), in-process. ERFA is
pure computation: no I/O, no file parsing, scalar-only inputs,
~25 years of heavy production use in safety-critical astronomy
software.

### Why in-process, not a managed service

Isolating ERFA as an `rp`-managed subprocess (the posture used for
ASTAP, see [Plate Solver](../services/plate-solver.md)) would route
every primitive call through HTTP. A single `get_next_target`
invocation can issue 20+ ephemeris calls; the latency cost outweighs
a defensive boundary the math itself doesn't need. The
[panic-safety contract](#panic-safety-and-degradation) below covers
the remaining "what if ERFA misbehaves" risk without crossing a
process boundary, and the trait wall makes service-extraction a
mechanical follow-up if a real ERFA-related fault is ever observed
in production.

### Beyond the trait: condition-parameterized observed transforms

Two inherent methods on `ErfarsEphemeris` (not trait methods — the
trait's consumers never need them, and adding required methods would
break every test double) serve callers that must control refraction,
such as `polar-align`:

```
alt_az_with_conditions(site, target_icrs, time, Option<RefractionConditions>)
    -> Result<AltAz, EphemerisError>
icrs_from_alt_az(site, observed_alt_az, time, Option<RefractionConditions>)
    -> Result<IcrsCoord, EphemerisError>
```

`RefractionConditions { pressure_hpa, temperature_c }` defaults to
the trait-documented amateur-rig values (1013.25 hPa, 10 °C; RH and
wavelength stay fixed at 50 % / 0.55 µm). `None` disables refraction
entirely — ERFA's `Refco` yields zero refraction constants for
non-positive pressure, so the transform is the pure geometric one.
The inverse rides ERFA's `Atoc13` with observed type `'A'`
(azimuth / zenith distance) and round-trips the forward transform to
sub-arcsecond accuracy (pinned by a unit test). The trait's `alt_az`
is `alt_az_with_conditions` with the default conditions.

### Operations not in ERFA's surface

Rise/set, twilight, transit, and meridian-flip ETA live in
`derived.rs` and close over the ERFA-supplied positions. Rise/set and
twilight are midpoint bisections, to a 1 s tolerance over a bracket of
at most 12 hours, on a sign-changing function ("target altitude minus
threshold" for rise/set, "sun altitude minus threshold" for twilight):
about 18 alt/az or Sun evaluations per bound. Transit is closed-form
from local sidereal time, refined by one Newton step against the LST
at the candidate time; the meridian-flip ETA is closed-form from the
hour angle.

The bisection helper short-circuits to `None` if either endpoint is
NaN, so NaN inputs propagate to `Option::None` rather than spinning
forever.

## Panic Safety and Degradation

`ErfarsEphemeris` is designed to never crash the calling service,
regardless of host-clock misconfiguration or upstream wrapper
inconsistencies. Three layers, from innermost to outermost:

1. **Documented ERFA failure paths.** `Dtf2d` (calendar →
   JD pair) returns `Err` for years outside ERFA's [-4799, +∞)
   range; chrono accepts years down to -262144, so this path is
   reachable from safe code. We log via `tracing::error!` and
   produce a NaN-filled `TimeJds`.
2. **Undocumented-but-typed ERFA failure paths.** `Utctai` and
   `Epv00` have `Err` variants in their Rust signatures, but in
   practice their underlying ERFA C functions never produce the
   codes that would map to `Err` (Dtf2d already filters the years
   that would trigger Utctai's internal `eraDat`; eraEpv00 only
   ever returns `0` or `+1` per its source). We still match `Err`
   defensively — returning `None` from small extracted helpers
   (`utctai_pair`, `epv00_heliocentric`) — so the code stays
   panic-free without `.expect`/`.unwrap`.
3. **`panic::catch_unwind` at the trait method boundary.** Every
   `impl Ephemeris for ErfarsEphemeris` method body runs inside
   `panic::catch_unwind` via a `run_with_guard` helper. If anything
   below panics — most likely the `erfars` wrapper's
   `unexpected_val_err!` macro firing on an undocumented ERFA
   return code, but also any other panic in the call tree — the
   payload is logged via `tracing::error!` and a method-appropriate
   NaN/None fallback is returned. The default panic hook still
   fires before catch, so operators see the panic message on
   stderr; the service stays up.

### Fallback values per method

| Method | Fallback on caught panic |
|---|---|
| `sidereal_time` | `LocalSiderealTime { lst_hours: NaN }` |
| `alt_az` | `Ok(AltAz { altitude_degrees: NaN, azimuth_degrees: NaN })` |
| `transit` | `None` |
| `rise_set` | `None` |
| `meridian_flip` | `None` |
| `sun_position` | NaN-filled `SunInfo` |
| `twilight` | `TwilightWindow { begin_utc: None, end_utc: None }` |
| `moon_position` | NaN-filled `MoonInfo` (incl. `phase_degrees`, `illumination_fraction`) |
| `moon_separation` | `f64::NAN` |

### What operators should do when they see NaN

NaN coords or all-`None` windows on the dashboard or via MCP tools
mean one of:

- **Host clock is misconfigured.** Most common cause — check
  `tracing::error!` output for an `ERFA Dtf2d rejected ...` line
  with a `time=...` field showing a date outside ERFA's calendar
  range.
- **Upstream wrapper inconsistency.** Rare — the `erfars` wrapper
  would have panicked inside an `unexpected_val_err!` arm and our
  `catch_unwind` layer caught it. Look for `panic_message=...`
  fields on `tracing::error!` lines with `method=...`. Action: open
  an upstream issue; pin a working `erfars` version.

The crate logs at `error` level, not `warn`, because every recovery
path here represents real misconfiguration or a real upstream bug —
not normal degraded operation.

## Thread Safety

The `Ephemeris` trait methods are pure functions and hold no mutable
state, so `ErfarsEphemeris` is `Send + Sync` and safe to share across
the `rp` service's tokio worker threads — with **one** caveat that
lives below the `erfars` boundary.

ERFA is otherwise reentrant, but it bolts a *user-updatable*
leap-second table onto SOFA (`external/erfa/src/erfadatextra.c`). Its
`eraDatini` lazily fills two file-static globals — the table pointer
`changes` and its length `NDAT` (sentinel `-1`) — on the **first**
`eraDat` call, with no lock and no atomics. If several threads make
their first `eraDat` call simultaneously (the cold-start case: nothing
has computed an ephemeris yet), one can read `NDAT > 0` without a
happens-before edge on `changes` and dereference a null/half-written
pointer, **segfaulting the whole process**. This is an upstream ERFA
thread-safety bug, not something the panic-safety layer can catch — a
SIGSEGV is not an unwindable Rust panic.

Every trait method reaches `eraDat` through `time_jds` (the UTC→TAI
conversion), so `time_jds` forces that first call to happen exactly
once, single-threaded, behind a `std::sync::Once`
(`ensure_erfa_leap_seconds_initialized`). `Once::call_once` blocks all
racing callers until the init completes; afterwards `NDAT` is positive
for the life of the process and concurrent callers only ever read it,
so the guard costs a single relaxed atomic load per call. The
`tests/leap_second_init_race.rs` integration test (its own binary, so
it owns the process's cold start) drives 64 threads onto their first
call at once, to catch the race should the guard ever go
([Testing](#testing)).

## Time-Scale Treatment

JDs are computed once per call as a `TimeJds` struct holding three
pairs:

- `utc1, utc2` — UTC JD pair from `eraDtf2d`.
- `tt1, tt2` — TT JD pair via `eraUtctai → eraTaitt`.
- `ut11, ut12` — UT1 JD pair, treated as identical to UTC.

ΔUT1 = 0 is the only time-scale simplification (UT1 ≈ UTC, error
≤ 0.9 s = ≤ 13″ of LST). The observed-place transforms, `Atco13`,
`Atoc13` and `Atio13`, and `Pvtob`, which places the observer for the
Moon's parallax, also get no polar motion (`xp = yp = 0`) and the site
at sea level (`hm = 0`), each worth well under an arcsecond of alt/az,
except sea level for the Moon from a high site: about 2″ at 4000 m.
These are well inside what plate-solving refines on a real frame, and well
below the positional accuracy any amateur rig delivers without
refinement.

The leapsecond table inside `erfars` is whatever shipped with the
pinned crate version. Beyond the table's release year + 5, `eraDat`
returns a `+1` warning that propagates as `Ok(_, 1)` from the
wrapper — we treat it as success. A long-running deployment that
outlives the leapsecond table will silently drift by the
accumulated leapsecond count; bumping `erfars` resyncs it.

## Coordinate Frames

- **Target inputs (`IcrsCoord`)** are ICRS (≈ J2000 mean
  equator/equinox to milliarcsecond, which is below any practical
  amateur-rig accuracy).
- **Geocentric Sun position** comes from `eraEpv00` (heliocentric
  Earth → negate → Sun direction from Earth). BCRS ≈ ICRS to
  milliarcsec, so the result is treated as ICRS.
- **Geocentric Moon position** comes from `eraMoon98`. GCRS ≈ ICRS
  to the same precision.
- **Topocentric alt/az** comes from `eraAtco13` with default
  amateur-rig refraction (1013.25 mb, 10 °C, 50 % RH, 0.55 µm).
  Mount-side refraction modelling supersedes this for slew
  pointing; our alt/az is for "is the target above the horizon"
  decisions, where refraction at the horizon (~34′) matters but
  the exact micro-modelling doesn't.
- **The Moon's topocentric alt/az** moves the Moon from the geocentre
  to the site first: its parallax, 54′–61′ at the horizon with its
  distance, is too large to leave out. `moon_alt_az_at` subtracts the
  observer's position (`eraPvtob`) from the Moon's (`eraMoon98`,
  rotated into CIRS by `eraC2i06a`) and hands the direction between
  them to `eraAtio13`, which takes a CIRS place as given and applies
  the same refraction. The result carries neither light-time, which
  `Moon98` omits, nor annual aberration, which `Atio13` does not apply:
  the Moon shares the Earth's orbital motion, so the two together move
  it by about an arcsecond, where aberration alone, as `Atco13` applies
  it to a star, would move it by about 20″.
- **The Moon's `coords` and `moon_separation`** stay geocentric, the
  same from every site; `moon_separation` takes no site. It differs
  from the separation seen at a site by up to the Moon's parallax,
  about 1°, which is small beside the tens of degrees a
  moon-separation limit asks for.

Annual aberration is not applied to the Sun's coordinates (sub-arcmin
effect; below the resolution that matters for "is the Sun up?"), nor
is the Sun's parallax (8.8″) applied to its alt/az.

### `IcrsCoord` vs the validated plan coordinate

This crate's `IcrsCoord` (in `types.rs`) is a **computed** value: the
transforms build it from ERFA output, and the [panic-safety
contract](#panic-safety-and-degradation) fills it with `NaN` on a bad host
clock (`sun_position`/`moon_position` degrade rather than crash). It is
deliberately **not** the validated plan coordinate
`rp_vocabulary::IcrsCoord`, which is private-field, `try_new`-checked to
`ra_hours ∈ [0,24)` / `dec_degrees ∈ [-90,90]`, and so cannot hold `NaN`
or a body that normalises exactly onto the `24.0h`≡`0h` seam — a genuinely
different concern (validated plan input vs computed astronomy output), a
false cognate rather than a duplicate.

The two are bridged in `vocabulary.rs`, and the direction asymmetry is the
whole point: `From<rp_vocabulary::IcrsCoord> for IcrsCoord` (plan →
computed, **total** — a validated coord is always a valid transform input)
and `TryFrom<IcrsCoord> for rp_vocabulary::IcrsCoord` (computed → plan,
**partial** — `NaN`/seam surface as a `CoordError`; a plain `From` here
would panic on the seam or clamp silently). See
[rp-vocabulary.md § Not this crate](rp-vocabulary.md#not-this-crate-false-cognates).

## Module Layout

```
crates/rp-ephemeris/src/
├── lib.rs          # crate root: trait Ephemeris + re-exports
├── types.rs        # IcrsCoord, AltAz, LocalSiderealTime, RiseSet,
│                   #   SunInfo, MoonInfo, TwilightKind, TwilightWindow,
│                   #   EphemerisError
├── site.rs         # Site + tzf-rs timezone resolution, night_date
├── erfars_impl.rs  # ErfarsEphemeris, time_jds, alt_az_at, sun_icrs,
│                   #   moon_icrs, moon_alt_az_at, run_with_guard,
│                   #   NaN-fallback ctors
├── vocabulary.rs   # From/TryFrom bridge between the computed IcrsCoord
│                   #   and the validated rp_vocabulary::IcrsCoord
└── derived.rs      # bisect_dt, transit, rise_set, meridian_flip,
                    #   twilight (root-finders over ERFA positions)
```

The trait lives in `lib.rs` rather than a dedicated `ephemeris.rs`
because it's the only public abstraction; co-locating it with the
re-exports keeps the crate root self-explanatory.

## Testing

- Unit tests live next to the code under `#[cfg(test)] mod tests`.
- `tests/reference_values.rs` compares `alt_az` against Astropy's
  values in `refvals/*.json`, within 1e-4°, for nine fixed objects at
  three sites and four times, and `moon_position`'s alt/az against
  Astropy's topocentric Moon from JPL's DE440s, within 10″, at the same
  sites and times. `Moon98`, a truncated lunar theory, agrees with
  DE440s to about 4″ there. `refvals/gen.py`, run by hand, writes
  those files, which are committed; the test fails when it finds none
  ([`refvals/README.md`](../../crates/rp-ephemeris/refvals/README.md)).
- `tests/leap_second_init_race.rs` is a standalone-binary
  concurrency regression test for the ERFA leap-second lazy-init race
  (see [Thread Safety](#thread-safety)): 64 threads make their first
  ephemeris call under a barrier. Without the `Once` warm-up that
  segfaults 20–40 % of cold starts on a 16-core host; with it the test
  is deterministically clean.
- `Site` tests cover lat/lon range validation, tz resolution
  (Seattle → America/Los_Angeles, Madrid → Europe/Madrid), and
  `night_date`'s roll at local noon.
- `ErfarsEphemeris` tests cover normal-case math (GMST at the J2000
  epoch; Polaris altitude at Seattle ≈ latitude; Sun on the vernal
  equinox ≈ RA 0/Dec 0; Sun below horizon at Seattle midnight in
  winter; the Moon's topocentric altitude below its geocentric one by
  its parallax), the kept `Dtf2d` Err path via year=-10000, end-to-end
  NaN/None degradation across all trait methods for an out-of-range
  year,
  and the [panic-safety contract](#panic-safety-and-degradation):
  - `run_with_guard` happy path and panic-recovery path
  - `panic_payload_message` for `&'static str`, `String`, and
    unknown-type payloads
  - the small extracted helpers `dtf2d_jds`, `utctai_pair`,
    `epv00_heliocentric` for both Ok and Err arms

This lets the structurally-unreachable Err arms (Utctai and Epv00
in practice never return Err) still be exercised in tests via the
helper seams, so the crate carries no `coverage(off)`-style
exclusions and no `.expect`/`.unwrap`/`unreachable!` in production
code.

## Dependencies

| Crate | Purpose |
|---|---|
| `chrono` | calendar/time arithmetic, `DateTime<Utc>` |
| `chrono-tz` | DST rules for `Site::night_date`'s local-time conversion |
| `erfars` | Rust FFI for ERFA (the math) |
| `rp-vocabulary` | validated plan `IcrsCoord`, for the `From`/`TryFrom` boundary bridge only |
| `tzf-rs` | offline lat/lon → IANA timezone, used by `Site` |
| `tracing` | error logging on degraded paths |
| `thiserror` | `EphemerisError` derive |
| `serde` | round-trip of public types over MCP |

`tzf-rs` ships an ODbL-licensed polygon dataset. Building the finder
takes the process to ≈46 MiB peak RSS (release build, x86_64 Linux) —
a whole-process figure, not the dataset's own allocation; see
[`docs/services/rp.md`](../services/rp.md#site-configuration) for
the deployment-posture rationale. The crate itself is MIT-licensed.

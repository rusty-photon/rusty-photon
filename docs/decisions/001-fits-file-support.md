# ADR-001: FITS File Support via fitsio Crate

## Status

Superseded twice. See **Amendments** below for the current state.

1. **Original (2024)**: chose `fitsio` (CFITSIO wrapper).
2. **First supersession**: replaced `fitsio` with `fitrs` (pure Rust)
   because CFITSIO failed to build on Windows MSVC (missing `pthread.h`).
   Pixel data widened from `u16` to `i32` because `fitrs` does not
   support `u16` writes — accepted as a small-file trade-off for guide
   star thumbnails.
3. **Second supersession (2026-05-01, Amendment A below)**: replacing
   `fitrs` with `fitsrs` (read) plus a hand-rolled pure-Rust writer
   inside `crates/rp-fits`. Drops the CFITSIO build dependency entirely,
   resolves a previously-undocumented GPL-3.0 license conflict, and
   restores native `u16` writes. Workload constraints from the QHY600
   sensor (~122 MB frames, parallel write throughput required) ruled
   out single-mutex CFITSIO serialisation; the hand-rolled writer is
   naturally parallel.

## Context

Rusty Photon is an astrophotography application that needs to read and write FITS (Flexible Image Transport System) files. FITS is the standard file format used in astronomy for storing images, tables, and metadata.

We needed to decide how to implement FITS file support in the workspace.

## Options Considered

### Option 1: Custom FFI Bindings

Build custom Rust bindings directly to NASA's CFITSIO C library.

**Pros:**
- Full control over the API surface
- Could optimize for specific use cases

**Cons:**
- Significant development effort
- Need to maintain FFI safety
- No clear benefit over existing solutions

### Option 2: Wrapper Crate

Create a thin wrapper crate around an existing FITS library to provide async compatibility.

**Pros:**
- Could provide a cleaner async API
- Centralized error handling

**Cons:**
- Unnecessary abstraction layer
- Async compatibility can be achieved via `spawn_blocking` at call sites
- Additional maintenance burden

### Option 3: Direct Dependency on fitsio Crate (Chosen)

Use the existing `fitsio` crate (v0.21) as a direct workspace dependency.

**Pros:**
- Well-maintained crate (latest release Sept 2025)
- Wraps NASA's CFITSIO library with safe Rust API
- Good Linux/macOS support (Tier 1)
- Minimal integration effort
- Active community and documentation

**Cons:**
- Limited Windows support (Tier 3/MSYS2)
- Synchronous API (requires `spawn_blocking` for async contexts)
- Dependency on system CFITSIO library

## Decision

We chose **Option 3: Direct dependency on fitsio crate**.

The `fitsio` crate provides a mature, well-tested wrapper around CFITSIO with good platform support for our primary targets (Linux and macOS). The synchronous API is acceptable since blocking I/O can be offloaded to a thread pool using `tokio::task::spawn_blocking` where needed.

## Consequences

### CI/CD Changes

All CI workflows must install the CFITSIO system library:
- Ubuntu: `sudo apt-get install -y libcfitsio-dev`
- macOS: `brew install cfitsio`
- Windows: Limited support, skipped in CI or requires MSYS2

### Usage Pattern

Services using fitsio in async contexts should wrap blocking calls:

```rust
use fitsio::FitsFile;

async fn read_fits_header(path: &str) -> Result<HeaderValue, Error> {
    let path = path.to_string();
    tokio::task::spawn_blocking(move || {
        let mut fptr = FitsFile::open(&path)?;
        let hdu = fptr.primary_hdu()?;
        // ... read data
        Ok(result)
    }).await?
}
```

### Platform Support

- **Linux**: Full support (Tier 1)
- **macOS**: Full support (Tier 1)
- **Windows**: Limited support (Tier 3) - requires MSYS2 or vendored builds

## Amendments

### Amendment A — 2026-05-01: Replace `fitrs` with `fitsrs` reads + hand-rolled writer

Issue [#107](https://github.com/rusty-photon/rusty-photon/issues/107)
audited the workspace's FITS surface and found the `fitrs` choice no
longer fit. **Three parallel spikes** evaluated successor strategies
across reads, writes, and a late-surfacing pure-Rust dual-purpose
candidate; a workload constraint (QHY600 sensor parallel-write
throughput) and a cross-project trust signal (one of `fitsrs`'s top
contributors is the author of `ascom-alpaca-rs`, which we already
depend on) shaped the final decision.

#### What changed since the first supersession

1. **Three FITS consumers, not one.** The original ADR-001 reasoned
   from "guide star thumbnails" alone. By 2026 the workspace has
   three distinct call sites:
   - `services/rp/src/persistence/fits.rs` — read + write + DOC_ID
     keyword for the disk-fallback resolver, currently `fitrs`.
   - `services/sky-survey-camera/src/fits.rs` — parse SkyView HTTP
     response bytes into pixel arrays, currently a hand-rolled
     ~200-line parser because `fitrs` was rejected for this consumer
     during PR #105 review.
   - `services/phd2-guider/src/fits.rs` — write u16 guide-star
     thumbnails, currently `fitrs` widening to i32 (the original
     trade-off).
2. **License conflict.** Workspace is `MIT OR Apache-2.0`
   (`Cargo.toml:22`); `fitrs` is **GPL-3.0**. This was previously
   undocumented and is a real distribution-license concern. Neither
   the original ADR nor the first supersession captured it.
3. **`fitrs` is unmaintained.** Last release 2019-10-31. PR #4
   ("Added 16 bits unsigned integer format", opened 2023-11-07) and
   three other PRs sit unanswered. The maintainer has not committed
   to the repository in 6.5 years.
4. **`fitrs` is functionally inadequate.** It does not apply
   `BSCALE`/`BZERO` on read (silently shifts SkyView's
   `BITPIX=16 + BZERO=32768` data by −32768), has no `Read`-based
   constructor (forces a tempfile-per-request hot path for
   sky-survey-camera), and surfaces pixels as `Option<i32>` because
   of the BLANK convention (rp's reader currently `filter_map`s the
   sentinel out, silently dropping pixels).
5. **QHY600 throughput requirement.** Production workloads include
   flat-calibration sessions on a QHY600 sensor (~122 MB per frame,
   30+ frames captured back-to-back). Writes need to overlap with
   the next exposure's readout to keep up with camera bandwidth.
   Single-mutex CFITSIO serialisation — the standard mitigation for
   non-reentrant CFITSIO — caps throughput at one write at a time
   and is incompatible with this workload.

#### Investigation: three spikes

| Spike | Branch / PR | Conclusion |
|---|---|---|
| **Read-side: `fitsrs`** | `chore/fits-spike` (spike doc/report not committed to `docs/plans/`) | Green. fitsrs covers all reads our consumers need: `Cursor<&[u8]>` API, BITPIX 8/16/32/-32/-64, multi-HDU, header keyword inspection, 7.7 ms parse for 300×300 BITPIX=16. Does NOT auto-apply BSCALE/BZERO/BLANK — caller must read header keys and apply scaling, which is fine because the wrapper crate becomes the one place that does so. **fitsrs has no writer (README marks `[ ] FITS writer/serializer` as unimplemented).** |
| **Write-side: stock `fitsio` + vcpkg CFITSIO on Windows MSVC** | `feature/fits-cfitsio-vcpkg-spike` ([PR #113](https://github.com/rusty-photon/rusty-photon/pull/113); spike doc/report not committed to `docs/plans/`) | Green technically. `vcpkg install --triplet x64-windows-static-md cfitsio` (no `[pthreads]` feature) + `choco install pkgconfiglite` + appropriate `PKG_CONFIG_*` env vars makes stock `fitsio = "0.21"` from crates.io build, link, and run on Windows MSVC with **no source patches anywhere**. Bypasses the [`simonrw/rust-fitsio#230`](https://github.com/simonrw/rust-fitsio/issues/230) blocker that originally forced the first supersession. **But the runtime is non-reentrant CFITSIO** — tests must run with `--test-threads=1`, and any production code path with concurrent FITS I/O must serialise via a process-wide Mutex. |
| **Pure-Rust read+write: `fitsio-pure`** | `feature/fitsio-pure-spike` ([PR #115](https://github.com/rusty-photon/rusty-photon/pull/115); spike doc/report not committed to `docs/plans/`) | Green across all three platforms. Pure-Rust dual-purpose crate published 2026-02-15. All 8 spike tests pass on `ubuntu-latest` (31 s), `macos-latest` (37 s), `windows-latest` (108 s) — no system deps anywhere. `read_image_physical` auto-applies BSCALE/BZERO. QHY600-scale 122 MB round-trip in 3.4-7.4 s wall-clock. Code quality is reasonable (zero `unsafe`, modern hand-rolled error type with source-chaining, 37 production unwraps, extensive defensive arithmetic). **But maintenance is concentrated in one author**: `meawoppl` is the only committer, the project is 3 months old, last commit was 2026-02-19 (71 days quiet at decision time), 1 GitHub star, 0 external contributors. |

The cfitsio-vcpkg spike succeeded but the resulting architecture
fails the QHY600 throughput requirement. Option 1 from the spike
analysis (PR upstream a conditional `USE_PTHREADS` patch) and
Option 2 (fork `fitsio-sys` and override via `[patch.crates-io]`)
have the same throughput limitation. Reentrant CFITSIO with
explicit `pthreads4w` linkage on Windows would technically allow
parallel writes but adds another layer of build-time complexity
(per-platform link directives, vcpkg `[pthreads]` feature, fitsio-sys
build.rs wrangling) for features our consumers don't otherwise need.

The fitsio-pure spike also succeeded technically — and would have
collapsed the wrapper crate to ~250 LOC instead of the ~600-800
that fitsrs+hand-rolled requires — but the maintenance-concentration
risk weighed heavier than the LOC savings. See **Decision logic**
below for how that trade-off was made.

#### NINA precedent

The [Nighttime Imaging 'N' Astronomy](https://nighttime-imaging.eu/)
suite (a real-world astrophotography application, ~10 years of
development, MPL-2.0) ships **both** code paths in the same
codebase:

- `NINA.Image/FileFormat/FITS/FITS.cs` and friends — a hand-rolled
  pure-C# writer (~1050 LOC across header serialisation + per-BITPIX
  data converters + atomic write helper).
- `NINA.Image/FileFormat/FITS/CFitsioFITS.cs` and friends — a
  CFITSIO-via-P/Invoke path bundling a custom-built `cfitsionative.dll`.

Dispatch is driven by `FITSCompressionTypeEnum`: the hand-rolled
writer handles `NONE` (the default); the CFITSIO path is engaged
only when the user opts into RICE/GZIP/HCOMPRESS/PLIO compression.
This validates the pattern of "hand-roll the simple case, bring in
CFITSIO only for features that demand it" as an engineering choice
that production astrophotography software has lived with for years.

#### Decision logic

After all three spikes were green, the choice between candidates
came down to a maintenance-risk vs. wrapper-LOC trade-off rather
than a feature-coverage trade-off. Each candidate was scored on
five axes:

| Candidate | License | Concurrent writes | Windows MSVC | Maintainer redundancy | Wrapper LOC |
|---|---|---|---|---|---|
| `fitrs` (status quo) | **GPL-3.0 ✗** | yes (no globals) | yes | dead since 2019 | small |
| `fitsio` + vcpkg CFITSIO | clean | **no — non-reentrant** | works but with vcpkg/`pkgconfiglite`/env-var setup | active upstream (slow on Windows-PR cadence) | ~150 LOC |
| `fitsio-pure` | clean | yes (pure Rust, no globals) | yes (no setup) | **1 author, 71 days quiet, 0 external contributors** | ~250 LOC |
| `fitsrs` + hand-rolled writer | clean | yes | yes (no setup) | **6 contributors incl. RReverser (48 commits) and bmatthieu3 (125 commits, cds-astro)** | ~600-800 LOC |

The first two candidates eliminate themselves: `fitrs` on license
plus inactivity, `fitsio` + vcpkg CFITSIO on QHY600 throughput
(non-reentrant CFITSIO requires single-mutex serialisation across
the entire process; calibrator-flats sessions write 30+ frames at
~122 MB each in quick succession with parallel disk bandwidth
required).

The last two are both technically viable. The deciding factor was
**maintainer redundancy**, weighted by a cross-project trust signal:

- **`RReverser` (Ingvar Stepanyan)** — Cloudflare engineer, author
  of [`ascom-alpaca-rs`](https://github.com/RReverser/ascom-alpaca-rs)
  which the workspace already depends on (forked at
  `ivonnyssen/ascom-alpaca-rs`). 48 commits to fitsrs across multiple
  feature areas (bintable, headers, refactoring). Code we already
  trust transitively in production.
- **`bmatthieu3` (Matthieu Baumann, cds-astro)** — primary fitsrs
  maintainer with 125 commits. Domain expert at a real astronomy
  data center.
- 4 other fitsrs contributors with smaller but non-trivial
  contributions.

`fitsio-pure` is excellent code (zero `unsafe`, modern error
handling, defensive arithmetic, 1741 lines of integration tests
including a 63-file mission FITS corpus) — but every line of it
was written by `meawoppl` in a 5-day burst (2026-02-15 to
2026-02-19) and they have not committed since. There is no second
pair of eyes upstream. We would be the first production adopter
visible on crates.io (566 lifetime downloads at decision time).

Trading roughly **400-500 LOC** of wrapper code we own for a
strictly stronger upstream is a defensible exchange:

- The hand-rolled writer surface is small and stable. FITS is a
  ratified standard from 1981 with stable BITPIX 8/16/32/-32/-64
  semantics. We are not chasing a moving target.
- NINA proves the architecture is sustainable for a decade-plus
  in production astrophotography software.
- The wrapper insulates us either way — if `meawoppl` returns
  with a 1.0 release we can revisit; if not, we have not bet on
  them.
- Conversely, `fitsrs`'s read-side is the most thoroughly-reviewed
  pure-Rust FITS reader in the ecosystem. Reads are exactly the
  surface area that is hard to get right (every BITPIX × BSCALE
  × BZERO × BLANK × tile-compression × WCS combination); we want
  the most-eyes-on code there.

Hence: **`fitsrs` reads + hand-rolled writer.**

#### Final architecture

A new workspace crate `crates/rp-fits` exposes the API surface our
three consumers need. Internally:

- **Reads delegate to `fitsrs`** (`reader` module). Thin facade that
  reads BSCALE/BZERO/BLANK from the header and applies scaling at
  our layer, returning typed pixel data to consumers.
- **Writes are hand-rolled in pure Rust** (`writer` module, ~400-500
  LOC). Supports BITPIX=8/16/32 image emission, custom keyword
  cards, BSCALE/BZERO writes for native `u16` via the
  `BITPIX=16 + BZERO=32768` convention, big-endian byte ordering,
  2880-byte block padding. Naturally parallel — each call owns its
  state, no shared globals.
- **Atomic-write helper** (`atomic` module, ~150 LOC). Lifted from
  the existing `services/rp/src/persistence/fits.rs:write_fits_sync`
  stage→fsync→rename→fsync-parent dance.
- **No CFITSIO** in the workspace. No vcpkg, no pkgconfiglite, no
  `--test-threads=1`, no per-platform link directives.

The cfitsio-vcpkg spike (branch
[`feature/fits-cfitsio-vcpkg-spike`](https://github.com/rusty-photon/rusty-photon/tree/feature/fits-cfitsio-vcpkg-spike))
and the fitsio-pure spike (branch
[`feature/fitsio-pure-spike`](https://github.com/rusty-photon/rusty-photon/tree/feature/fitsio-pure-spike))
remain on their dedicated branches as historical evidence — if a
future consumer demands BINTABLE / WCS / RICE compression, the recipe
to add CFITSIO back without source patches is captured there; if
`fitsio-pure` matures into a multi-contributor project, swapping the
wrapper crate's writer implementation for it is a contained delta.
Neither spike crate ships in `main` to keep the workspace lean.

#### Consequences

##### Workspace dependencies

- **Add**: `fitsrs = "0.4"` as a workspace dependency (used by
  `crates/rp-fits`).
- **Remove**: `fitrs = "0.5.0"` from the workspace `Cargo.toml`.
  All current consumers migrate to `crates/rp-fits`.
- **Remove**: any reliance on `libcfitsio-dev` / `cfitsio` Homebrew
  / vcpkg `cfitsio` for normal workspace builds. CI loses the
  per-platform CFITSIO install steps.
- **License clean by construction.** All workspace dependencies
  are now MIT/Apache-2.0 or BSD-3-Clause. The previously-silent
  GPL-3.0 contamination via `fitrs` is resolved.

##### Service migrations (in suggested order)

1. **`services/sky-survey-camera`** — first migration. Read-only,
   biggest immediate win: drops the ~200-line hand-rolled
   `parse_primary_hdu` in favour of `rp_fits::reader`. The
   `Cursor<&[u8]>` API path is preserved.
2. **`services/phd2-guider`** — second migration. Replaces
   `write_grayscale_u16_fits` with `rp_fits::writer::write_u16_image`
   (or equivalent). Restores native `u16` writes — original
   ADR-001's "widen to i32 because fitrs can't write u16"
   compromise reverts.
3. **`services/rp/src/persistence`** — third migration. Largest
   surface: read + write + DOC_ID keyword. Atomic-write dance
   moves into `rp_fits::atomic`; the rest of the persistence
   module shrinks accordingly.

##### Operational

- **No Windows MSVC contributor friction.** Pure Rust everywhere
  — no `vcpkg install`, no `pkgconfiglite`, no PKG_CONFIG_* env
  vars, no `--test-threads=1`. `cargo build` works on a fresh
  laptop with the standard `dtolnay/rust-toolchain@stable`.
- **Native `u16` writes** restore correct byte-level
  representation. phd2-guider's guide-star thumbnails and rp's
  16-bit captures (the QHY600-class case) both stop widening to
  i32. **rp's on-disk format moves from BITPIX=32 to BITPIX=16**
  (with `BSCALE=1, BZERO=32768`) for the common path; cameras
  whose `max_adu` exceeds `u16::MAX` fall through to BITPIX=32 so
  high-bit-depth scientific sensors stay lossless. `read_fits_pixels`
  continues to return `Vec<i32>` regardless of on-disk encoding,
  so imaging code is unaffected.
- **Parallel writes** on QHY600 workloads are unblocked. The
  hand-rolled writer has no shared global state.
- **Test serialisation requirement disappears.** The non-reentrant
  CFITSIO race conditions that surfaced in the cfitsio-vcpkg spike
  are inapplicable.
- **Bounded scope.** The hand-rolled writer covers our consumers'
  needs. We are not signing up to be a general-purpose FITS
  library author. Future BINTABLE / WCS / compression needs would
  require a separate decision (and likely re-engaging the
  cfitsio-vcpkg path captured in the spike).

##### What we explicitly do not pursue

- **Forking `fitsio-sys`** or contributing the conditional
  `USE_PTHREADS` patch upstream. Captured in the cfitsio-vcpkg
  spike report; not needed once we drop CFITSIO from the workspace.
- **Forking `fitrs`** to relicense or add `u16` writes. Maintainer
  is unresponsive; license is contagious.
- **Forking `fitsrs` to add a writer.** The maintainer is highly
  responsive (15+ external PRs merged in 2025-26) but our writer
  surface is small enough that the wrapper-crate path is cheaper
  than upstream coordination.
- **Adopting `fitsio-pure` for both reads and writes.** Code quality
  is reasonable and the spike was clean across all three platforms.
  The blocker is maintainer concentration — single author, 71 days
  silent at decision time, no external review surface. Re-evaluate
  if/when the project gains a second active contributor or visibly
  resumes development.

## References

### Crates considered

- [`fitsio` crate](https://crates.io/crates/fitsio) — CFITSIO
  bindings; ruled out under Amendment A on QHY600 throughput
  (non-reentrant CFITSIO).
- [`fitrs` crate](https://crates.io/crates/fitrs) — GPL-3.0,
  unmaintained since 2019; subject of removal under Amendment A.
- [`fitsrs` crate](https://crates.io/crates/fitsrs) — MIT/Apache-2.0,
  active multi-contributor pure Rust; **chosen reader under
  Amendment A**.
- [`fitsio-pure` crate](https://crates.io/crates/fitsio-pure) —
  Apache-2.0 pure-Rust reader+writer; spike-green but ruled out on
  maintainer concentration; preserved as a future-revisit option.

### Spike artifacts (Amendment A)

Spike docs/reports were not committed to `docs/plans/`; see the branches and
PRs in the investigation table above (`chore/fits-spike`,
[PR #113](https://github.com/rusty-photon/rusty-photon/pull/113),
[PR #115](https://github.com/rusty-photon/rusty-photon/pull/115)).

### External

- [NASA CFITSIO](https://heasarc.gsfc.nasa.gov/fitsio/)
- [FITS Standard](https://fits.gsfc.nasa.gov/fits_standard.html)
- [N.I.N.A. on Bitbucket (canonical)](https://bitbucket.org/Isbeorn/nina) —
  precedent for hand-rolled writer + optional CFITSIO architecture
- [`RReverser/ascom-alpaca-rs`](https://github.com/RReverser/ascom-alpaca-rs) —
  cross-project trust signal: the workspace already depends on this
  fork; its author is fitsrs's #2 contributor.
- [`simonrw/rust-fitsio#230 "MSVC?"`](https://github.com/simonrw/rust-fitsio/issues/230) —
  the long-running issue that drove ADR-001's first supersession;
  bypassable via the cfitsio-vcpkg spike's recipe but not chosen for
  workload reasons.
- [Issue #107: Pick a workspace FITS library](https://github.com/rusty-photon/rusty-photon/issues/107)
- [`docs/plans/archive/fits-library-consolidation.md`](../plans/archive/fits-library-consolidation.md)

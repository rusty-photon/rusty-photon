# Plan: Image evaluation tools in rp

**Status: COMPLETE (archived 2026-05-09).** All seven phases shipped;
the only items not addressed are the explicitly-deferred entries
under [Out of scope / deferred](#out-of-scope--deferred).

**Date:** 2026-04-28 (plan opened)
**Closed:** 2026-05-09
**Branch:** `worktree-image-evaluation-tools`

## Status snapshot

| Phase | Subject | Status |
|-------|---------|--------|
| 1 | Design doc updates | complete |
| 2 | BDD scenarios for `measure_basic` | complete |
| 3 | `imaging/` module promotion + image cache | complete |
| 4 | Implement `measure_basic` | complete |
| 5 | Subsequent image-analysis tools (`estimate_background`, `detect_stars`, `measure_stars`, `compute_snr`) | complete |
| 6 prep | Module reorg (imaging vs. persistence) | complete |
| 6a | Focuser primitives | complete |
| 6b | `auto_focus` design + BDD + impl | complete |
| 6c-prep | Mount primitives | complete |
| 6c-1 | `plate-solver` rp-managed service (separate plan: [`plate-solver.md`](plate-solver.md)) | complete |
| 6c-2 | `plate_solve` built-in MCP tool | complete |
| 6c-3 | `center_on_target` design + BDD + impl | complete |
| 7 | Unified document/image cache + UUID-suffixed filenames | complete |

The only deferred items remaining are documented under
[Out of scope / deferred](#out-of-scope--deferred). The rest of the
plan below is preserved for historical context — phase status banners
inside each section reflect the as-of date the work landed.

## Background

When the plan opened (2026-04-28) `rp` shipped only `compute_image_stats`.
The MCP catalog listed `measure_basic` (HFR, star count, background) but
it was not implemented, and there was no path for the broader image-
evaluation toolkit needed by focus, centering, and quality-screening
workflows. This plan added those tools as **built-in** capabilities of
`rp` per the "batteries included" architecture clarified during design
— see `docs/services/rp.md` (Component Categories and Image Analysis
Strategy).

The full toolkit (`measure_basic`, `estimate_background`,
`detect_stars`, `measure_stars`, `compute_snr`, `plate_solve`, plus
compound tools `auto_focus` and `center_on_target`) was defined in
`rp.md` as planned at that point. This plan sequenced and shipped them.

## Goals

1. **MVP:** ship `measure_basic` end-to-end: design → BDD → implementation,
   with the supporting `imaging/` module structure and image cache.
2. Establish the patterns (Pixel trait, cache enum, document-section
   persistence, BDD style) that subsequent tools will reuse.
3. Land subsequent image-analysis tools incrementally, each as its own PR.
4. Land compound built-in tools (`auto_focus`, `center_on_target`) once the
   primitives they need exist.

## Decisions resolved (during design)

These are now in `docs/services/rp.md` and should not be re-litigated:

- **Component categories:** built-in / rp-managed services / plugins.
  Plugin mechanism serves both first-party workflow logic
  (`calibrator-flats`) and third-party extensibility.
- **Cache wire format:** ASCOM Alpaca ImageBytes (type-tagged 44-byte
  header + raw pixels). No `/fits` endpoint — consumers needing FITS
  bytes read the file directly via the path in the exposure document.
- **Cache storage type:** `CachedPixels::U16 | I32` enum from day one.
  `u16` is the primary path (every camera rusty-photon will encounter is
  ≤ 16-bit non-negative); `I32` is a hatch for future scientific cameras
  (Andor, Hamamatsu sCMOS HDR). Selection is per-camera at connect time
  based on `MaxADU`, not per-frame.
- **Cache eviction:** LRU, configured in MiB (`cache_max_mib`) plus an
  image-count safety net (`cache_max_images`). Whichever trips first.
- **FWHM fitting crate:** `rmpfit` (lighter deps, native parameter
  bounds, MPFIT astronomy heritage). Not `levenberg-marquardt`.
- **Module structure:** `imaging.rs` → `imaging/` with submodules
  (`mod.rs`, `pixel.rs`, `fits.rs`, `cache.rs`, `stats.rs`,
  `background.rs`, `stars.rs`, `hfr.rs`, `fwhm.rs`, `snr.rs`,
  `measure_basic.rs`).

## Phases

### Phase 1 — Design doc updates ✓

Status: **complete on this branch.**

- [x] Component Categories section in Architecture
- [x] Tool Catalog reframed as three sources
- [x] Built-in Tools tables expanded with image-analysis and compound rows
- [x] Image Analysis Strategy: rmpfit pinned, `measure_basic` MVP contract
- [x] Image Cache section: internal API (`CachedPixels` enum), HTTP API,
      MiB-based eviction, ImageBytes wire choice
- [x] Plugin-Provided Tools and Plugin Types reframed (first-party vs
      third-party)
- [x] Plate Solver and Guider Service relabeled as rp-managed services
- [x] Compound Tools section reframed as in-process pattern
- [x] Configuration: removed vcurve-focus / iterative-centering from the
      plugin list; added `imaging.cache_max_mib` and `cache_max_images`
- [x] API Layer: added `/api/images/{document_id}` and `/pixels`
- [x] Module Structure updated for `imaging/`

### Phase 2 — BDD scenarios for `measure_basic`

Status: **complete.**

- [x] `services/rp/tests/features/measure_basic.feature` (8 scenarios:
      catalog, image_path happy path, document_id happy path, document
      section persistence, high-threshold zero-stars, three error paths)
- [x] `services/rp/tests/bdd/steps/measure_basic_steps.rs` — step
      definitions reusing shared steps from `tool_steps.rs` (capture,
      list tools, error assertions)
- [x] `RpWorld` additions: `last_measure_basic_result`,
      `last_exposure_document` (for the section-fetch step)
- [x] Wired into `tests/bdd/steps/mod.rs`
- [x] `@wip` tag on the feature + `filter_run` in `bdd.rs` — keeps the
      default suite green until Phase 4 implementation lands. The
      convention is documented in `docs/skills/testing.md` §2.6 and
      `docs/skills/development-workflow.md` Phase 2d. **Removing the
      `@wip` tag is the explicit Phase 4 completion milestone.**

**Exit criteria met:** `cargo build --all-features --all-targets -p rp`
clean; `cargo test --all-features --test bdd -p rp` passes 82/82
non-`@wip` scenarios. The 8 `measure_basic` scenarios are filtered out
and will fail correctly once enabled in Phase 4.

### Phase 3 — `imaging/` module promotion + image cache

Status: **complete.**

- [x] Crate adds: `ndarray-ndimage` (workspace dep — used by stars,
      background, smoothing). Update `Cargo.toml` (workspace + rp).
      Run `CARGO_BAZEL_REPIN=1 bazel mod tidy` per CLAUDE.md rule 10.
- [x] Promote `services/rp/src/imaging.rs` → `services/rp/src/imaging/`.
      Move existing `compute_stats`, `write_fits`, `read_fits_pixels`
      into `stats.rs` / `fits.rs`. Keep public API stable.
- [x] `pixel.rs`: `Pixel` trait with `u16` and `i32` impls.
- [x] `cache.rs`: `CachedPixels` enum, `CachedImage` struct,
      `ImageCache` with LRU and MiB-based eviction. Internal-only at
      this stage.
- [x] Capture path (`mcp.rs:capture`) inserts into the cache after FITS
      write, narrowing to `u16` when `max_adu ≤ 65535`. If `max_adu`
      can't be read, cache insert is skipped (FITS-on-disk fallback
      still works).
- [x] `imaging` config block: `cache_max_mib`, `cache_max_images`,
      with sensible Pi-5 defaults (1024 MiB / 8 images).

**Exit criteria met:** capture populates the cache; `compute_image_stats`
still passes its existing BDD (12 features / 82 scenarios green);
`cargo rail run --merge-base -q --` clean; `cargo clippy -D warnings`
clean. `Pixel` trait and `CachedPixels::I32` are wired but unused —
they exist for Phase 4 (`measure_basic`) and the future scientific
camera hatch respectively. max_adu is fetched per-capture rather than
stashed at connect time — see follow-up note below.

#### Phase 3 follow-up: stash `max_adu` on `CameraEntry` — superseded

**Status:** **superseded** by the document-side `max_adu` field added
ahead of Phase 7.

**What changed.** `ExposureDocument` now carries `max_adu: Option<u32>`,
populated at capture time from a single `cam.max_adu().await` whose
result also drives the U16/I32 cache variant choice. The sidecar JSON
preserves the value across eviction and `rp` restart, so Phase 7's
disk-fallback rehydration is self-describing without needing the
originating camera to be connected.

**Why no `CameraEntry` stash.** With `max_adu` on the document, a
connect-time stash adds no value:

- **Capture path:** the live per-frame fetch already feeds both the
  cache variant decision and the doc field. One Alpaca call, one
  source of truth for that capture.
- **`get_camera_info`:** a pure capability query with no document
  context — its live fetch is appropriate and was never a hot-path
  concern.
- **Robustness is actually better the live way.** A connect-time read
  failure in a stashed model would force `max_adu = None` on every
  capture for the rest of the session. The live model isolates
  transient failures to the single affected capture; the next capture
  re-reads independently.

The original design statement in `docs/services/rp.md` ("read at
connect time and stash") is updated to reflect the per-capture +
sidecar pattern.

### Phase 4 — Implement `measure_basic`

Status: **complete.**

#### Decisions resolved during Phase 4 planning

These are now in `docs/services/rp.md` (MVP `measure_basic` Contract)
and should not be re-litigated:

- **Saturation: flag, don't reject.** Saturated stars carry real signal.
  Bright in-focus stars routinely clip at long exposures, and donut PSFs
  at extreme defocus saturate in their bright annulus. Rejecting them
  would make HFR-vs-focus non-monotonic and break auto-focus. Output
  carries `saturated_star_count` so downstream consumers (focus runs,
  quality screens) apply their own policy.
- **`min_area` / `max_area`: required parameters, no defaults.** Pixel
  area encodes a pixel-scale (arcsec/px) assumption that the tool cannot
  infer from the image alone. `threshold_sigma` keeps its default of
  `5.0` — it's unit-free (multiples of background stddev) and so
  scale-independent.
- **Connected components: hand-rolled 4-connectivity BFS.**
  `ndarray-ndimage` 0.6's `label` is 3D-only with a hard `assert!` on a
  3×3×3 structuring element. Wrapping the 2D mask via `insert_axis` is
  possible but yields a labels grid we'd then re-walk into per-component
  pixel lists anyway — and per-component pixel lists are exactly what
  centroiding and HFR want. A direct BFS producing
  `Vec<Vec<(usize, usize)>>` is the smaller total diff. The crate is
  still used for `gaussian_filter`.
- **`ImageBytesMetadata` layout replicated locally.**
  `ascom-alpaca`'s struct is `pub(crate)`. The 11×i32 LE header is
  small and well-defined; we replicate it in `routes.rs` with a unit
  test pinning the byte layout.
- **Exposure document store does not exist yet.** Before Phase 4 there
  is no `ExposureDocument` type, no in-memory store, no
  `GET /api/documents/{id}`. The original Phase 4 sketch assumed
  persistence existed; it does not. Phase 4 builds the foundation as
  Step 1 so subsequent persistence work has a place to land. Reload
  from sidecars on process restart is a Phase 5 follow-up.

#### Work breakdown (in order)

- [x] **Step 1 — Exposure document store.** New
      `services/rp/src/document.rs` with `ExposureDocument`,
      `DocumentStore::{create, get, put_section}`. Atomic sidecar JSON
      write (`<fits>.json.tmp` → rename) next to each FITS file.
      `mcp.rs:capture` constructs the document after FITS write +
      cache insert. New route
      `GET /api/documents/{document_id}` in `routes.rs` (the BDD
      step at `measure_basic_steps.rs:79` fetches this).
- [x] **Step 2 — `imaging/background.rs`.** Sigma-clipped
      mean/stddev/median over a `Pixel`-generic `ArrayView2`. Iterative
      clip (k=3, max_iters=5) with median via `select_nth_unstable` on
      the surviving set.
- [x] **Step 3 — `imaging/stars.rs`.**
      `gaussian_filter` (σ ≈ 1.0 px) → threshold → 4-connectivity BFS
      labelling → filter (area in `[min_area, max_area]`, border
      rejection — *no* saturation rejection) → intensity-weighted
      centroiding using background-subtracted flux. `Star` carries
      `saturated_pixel_count: u32`.
- [x] **Step 4 — `imaging/hfr.rs`.** Per-star radial flux accumulation
      to half of total flux, with sub-pixel linear interpolation between
      bracketing pixels. `aggregate_hfr` returns the median of per-star
      HFRs; `None` when no stars.
- [x] **Step 5 — `imaging/measure_basic.rs`.** Composes the above into
      `MeasureBasicResult { hfr, star_count, saturated_star_count,
      background_mean, background_stddev, pixel_count }`.
- [x] **Step 6 — MCP tool wiring in `mcp.rs`.** `MeasureBasicParams`
      with `min_area: Option<usize>` / `max_area: Option<usize>`
      (`#[serde(default)]`), optional `threshold_sigma: f64` (default
      5.0), and exclusive `document_id` / `image_path`. The area params
      are required-but-validated-in-body so the tool can produce error
      messages in deterministic input order: `document_id`/`image_path`
      first (the "missing both" error mentions `image_path` per
      `measure_basic.feature:78`), then `min_area`, then `max_area`. If
      the area fields were strictly required at the serde level, serde
      would error first on whichever it deserializes first, breaking
      the error-message ordering. Resolution order: cache hit →
      FITS-on-disk fallback via document `file_path` → error. After
      successful analysis with `document_id`, write the `image_analysis`
      section via `DocumentStore::put_section`.
- [x] **Step 7 — HTTP image endpoints in `routes.rs`.**
      `GET /api/images/{document_id}` (JSON metadata) and
      `/pixels` (`application/imagebytes`: 44-byte header where
      `transmission_element_type` = 8 for `U16`, 2 for `I32`; pixel
      bytes serialized via per-element `to_le_bytes` rather than a
      `bytemuck` cast — avoids adding a new workspace crate). Cache
      miss falls back to FITS decode + serve. *Note: not exercised by
      the 8 `measure_basic` BDD scenarios.*
- [x] **Step 8 — Activate `measure_basic.feature` and round out tests.**
      Remove `@wip` from line 1. Extend `read_fits_pixels` to also
      return `(width, height)` (one caller — `compute_image_stats` —
      updated alongside) so the FITS fallback path can reconstruct
      `Array2`. Bake test-fixture `min_area` / `max_area` into the
      step helper for the OmniSim image. Add unit tests on
      background/stars/hfr/measure_basic with exact-value assertions
      per `docs/skills/testing.md` §1.2.

`rmpfit` is **not** added in this phase — it's deferred to Phase 5
(`measure_stars` / FWHM).

**Exit criteria met:** 90/90 BDD scenarios pass (was 82/82 + 8
`@wip`); 33 new unit tests across `document`, `background`, `stars`,
`hfr`, `measure_basic`, plus 2 in `routes` for ImageBytes header
layout — 101 lib tests total, all green. `cargo rail run --merge-base`
exits 0 with no warnings. `cargo fmt` clean. No new workspace deps,
so no `bazel mod tidy` was needed.

### Phase 5 — Subsequent image-analysis tools

One PR per tool, in this order:

- [x] `estimate_background` (sigma-clipped mean / stddev / median; reuses
      `imaging/background.rs` kernel from Phase 4). Optional `k` (default
      3.0) and `max_iters` (default 5) clip parameters; persists into the
      exposure document as a `background` section, separate from
      `measure_basic`'s `image_analysis`. 10 BDD scenarios (catalog,
      image_path / document_id happy paths, persistence, custom k+iters,
      five error paths). 100/100 BDD scenarios green.
- [x] `detect_stars` (per-star list with `{x, y, flux, peak,
      saturated_pixel_count}` plus aggregate counts and the sigma-clipped
      background used for the threshold). Reuses `imaging/stars.rs` and
      `imaging/background.rs` kernels from Phase 4. `peak: f64` (raw, not
      background-subtracted) was added to `Star` for this tool and to give
      `measure_stars` an FWHM-fit initial guess. Persists into the
      exposure document as a `detected_stars` section. 10 BDD scenarios
      (catalog, image_path / document_id happy paths, persistence,
      high-threshold empty list, five error paths). 110/110 BDD scenarios
      green.
- [x] `measure_stars` (per-star HFR, FWHM, eccentricity, flux + median
      aggregates and the sigma-clipped background). Composes
      `imaging/background.rs` + `imaging/stars.rs` + `imaging/hfr.rs` with
      a new `imaging/fwhm.rs` (asymmetric 2D Gaussian fit, no rotation,
      6 parameters via `rmpfit` Levenberg-Marquardt). FWHM = 2.3548·√(σx·σy);
      eccentricity = √(1 − (σmin/σmax)²). Stars whose stamp would cross
      the image edge keep their row with `fwhm`/`eccentricity` set to
      `null` rather than being dropped. Persists into the exposure
      document as a `measured_stars` section. The optional `stars` input
      from the catalog row is deferred (always re-runs detection); it
      will let callers pass back a previous `detect_stars` array to skip
      detection. 10 BDD scenarios (catalog, image_path / document_id
      happy paths, persistence, very-high-threshold empty list, five
      error paths). 120/120 BDD scenarios green; cargo rail run
      --merge-base clean. Adds `rmpfit = "1.0"` to workspace +
      services/rp Cargo.toml; `CARGO_BAZEL_REPIN=1 bazel mod tidy`
      refreshed `MODULE.bazel.lock`.
- [x] `compute_snr` (median per-star SNR via the CCD-equation
      approximation: `noise = √(signal + N_pix · σ_bg²)`). Composes
      `imaging/background.rs` + `imaging/stars.rs` from Phase 4 with a
      new `imaging/snr.rs` (per-star + median aggregator). Persists
      into the exposure document as an `snr` section. Documented
      caveat: the noise model collapses dark + read noise into the
      background variance and assumes gain ≈ 1 ADU/electron — values
      are comparable across frames from the same camera, not absolute
      photometric SNRs. 10 BDD scenarios (catalog,
      image_path / document_id happy paths, persistence,
      very-high-threshold null aggregates, five error paths). 130/130
      BDD scenarios green; cargo rail run --merge-base clean.

Each tool follows the same shape: design doc already covers it →
BDD feature file → step defs → unit tests → impl.

### Phase 6 — Compound built-in tools

`auto_focus` and `center_on_target` stay built-in (per design tenet —
"batteries included"; pure Rust math, no external program to wrap).
Pluggability is provided by **shadow semantics**: a tool-provider plugin
that advertises the same tool name overrides the built-in at startup
(logged). See `docs/services/rp.md` → Config-Time Validation and
Third-party alternatives for the rule. The shadow-rule doc edit lands
ahead of Phase 6a; nothing in `rp` itself needs to change to support it
beyond the catalog-merge logic that will be written when the first
plugin shadows a built-in (so far there is no caller, so no code yet).

#### Phase 6 prep — Module reorg (imaging vs. persistence)

Status: **complete.**

Before Phase 6 lands its first new files (`auto_focus.rs`,
`center_on_target.rs`), the `imaging/` module was split to give the
new tools and the planned filename-template work clear homes:

- `imaging/analysis/` — pure single-purpose kernels (`background`,
  `stars`, `hfr`, `fwhm`, `snr`, `stats`, `pixel`). Generic over
  `Pixel`, take `ArrayView2`, no async, no I/O — unit-testable
  without a runtime.
- `imaging/tools/` — compositional analyzers (`measure_basic`,
  `measure_stars`; `auto_focus` landed here in Phase 6b,
  `center_on_target` in Phase 6c-3). Each binds multiple kernels
  into one MCP-tool-shaped result.
- `persistence/` — the I/O / async / on-disk layer. `cache.rs`,
  `fits.rs`, and `document.rs` (moved from the crate root). Future
  filename-template / token-resolver work attaches here, not under
  `imaging/`.

`imaging/mod.rs` re-exports the flat `crate::imaging::<symbol>` shape
so MCP wiring doesn't have to know which submodule a definition lives
in. Persistence symbols (`ImageCache`, `CachedPixels`, `CachedImage`,
`ExposureDocument`, `write_fits`, etc.) move to `crate::persistence::*`
— callers updated accordingly. 162 lib tests + 139 BDD scenarios green
post-reorg; no behavior change. See the new Module Structure block in
`docs/services/rp.md` for the full layout.

#### Phase 6a — Focuser primitives (prerequisite for `auto_focus`)

Status: **complete.**

- [x] `FocuserConfig` in `config.rs` (replaces the `Vec<Value>`
      placeholder — `id`, optional `camera_id`, `alpaca_url`,
      `device_number`, optional `auth: ClientAuthConfig`, optional
      `min_position` / `max_position` bounds).
- [x] `FocuserEntry` in `equipment.rs` mirroring `CameraEntry` +
      `connect_focuser` Alpaca client wiring.
- [x] `EquipmentRegistry::find_focuser`.
- [x] MCP tools in `mcp.rs`: `move_focuser` (absolute position,
      bounds-checked against operator-supplied
      `min_position`/`max_position`, blocks via 100 ms `is_moving`
      polling with a 120 s deadline), `get_focuser_position`,
      `get_focuser_temperature` (calls `temperature()` directly and
      returns `temperature_c: null` only when the device returns
      `ASCOMError::NOT_IMPLEMENTED`; `Temperature` and
      `TempCompAvailable` are independent ASCOM properties — qhy-focuser
      reports `TempCompAvailable=false` but exposes a real temperature
      reading, and that case must surface the value, not null).
- [x] `services/rp/tests/features/focuser.feature` (11 scenarios:
      catalog, move/idempotent move/position/temperature happy paths,
      five error paths — focuser not found, not connected, below min,
      above max, missing focuser_id, plus get_focuser_position not
      found).
- [x] `services/rp/tests/bdd/steps/focuser_steps.rs` reusing
      `tool_steps.rs` shared steps; `RpWorld.focusers` accumulator;
      new `bdd_infra::rp_harness::FocuserConfig` + builder.
- [x] `docs/services/rp.md` Configuration example focuser block
      shows the typed schema with optional `min_position` /
      `max_position` and `auth`.
- [x] No `bazel mod tidy` needed — only added the existing
      `ascom-alpaca` `focuser` feature on the rp crate; no new
      workspace deps.
- [x] Direct unit tests for `connect_focuser` failure + success
      branches via an in-test axum stub Alpaca server (one of the
      paths under workspace-wide-strategy issue #111). Covers all
      five failure arms (`build_alpaca_client` error, `get_devices`
      network error, 5 s timeout, no-focuser-in-list, `set_connected`
      Alpaca rejection) plus the `Ok(())` success arm and
      `EquipmentRegistry::new` / `find_focuser` /
      `EquipmentStatus.focusers` plumbing.

**Exit criteria met:** 11 new BDD scenarios green (150/150 total
in rp's BDD suite); 24 new lib tests across `config` (2),
`equipment` (7 — 5 for `connect_focuser` failure paths, 1 success
path, 1 registry end-to-end via the axum stub), and `mcp` focuser
tools (15). `cargo rail run --profile commit -q` reports 600/600
tests passing workspace-wide. `cargo fmt` clean.

#### Phase 6b — `auto_focus` design + BDD + impl

Status: **complete.**

- [x] **Design:** `auto_focus` Contract added to `rp.md` Compound
      Tools (parallel to `measure_basic` Contract). Decisions
      resolved during design:
      - **Sweep range:** ± steps from `current_position`
        (`half_width: i32`), clamped to operator-supplied
        `min_position`/`max_position` from the `FocuserConfig`.
        Out-of-range grid points are dropped (not coerced) so the
        sweep does not produce duplicate samples at the bound.
      - **Step size:** required parameter, fixed across the sweep
        (no adaptive zoom in V1 — adaptive doubles BDD complexity
        for marginal benefit on amateur rigs; can ship later as a
        shadow tool).
      - **Exposure duration:** required `humantime` parameter (no
        default, no probe-derived heuristic — a probe that itself
        runs at unknown focus is unreliable as a driver for the
        rest of the sweep).
      - **V-curve fit:** parabolic in raw HFR, weighted by
        `star_count`. Closed-form least-squares — no rmpfit needed.
        Asymmetric-V / piecewise-linear listed as a future shadow
        alternative.
      - **Abort policy on starless captures:** skip-and-continue
        — record `hfr: null` in `curve_points` but exclude from
        the fit. Fail with `not_enough_stars` only if fewer than
        `min_fit_points` (default `5`) survive.
      - **Retry policy on monotonic curve:** none — fail with a
        `monotonic_curve` error and let the caller widen
        `half_width` or coarse-focus externally before retrying.
        No automatic move to the lowest observed sample.
      - **Output shape:** `{best_position, best_hfr,
        final_position, samples_used, curve_points,
        temperature_c}`. `curve_points[i]` is
        `{position, hfr|null, star_count, document_id}` so callers
        can fetch per-step provenance via the existing document
        API.
      - **Persistence:** `auto_focus` does *not* write a section
        on any single document. Each per-step capture's
        `image_analysis` section is written by the embedded
        `measure_basic` call as it normally would be. The
        compound result is returned in the MCP response and
        emitted as `focus_complete`. The `focus_complete` event
        payload is enriched with `focuser_id` and `samples_used`.
      - **`min_area` / `max_area` pass-through:** required at the
        `auto_focus` level, same as `measure_basic` — pixel scale
        varies per rig.
- [x] `services/rp/tests/features/auto_focus.feature` — 17 scenarios:
      catalog, four device-resolution errors (nonexistent and
      disconnected camera/focuser), seven missing-parameter
      examples via Scenario Outline, three numeric-range rejections
      (step_size=0, half_width=0, min_fit_points<3), grid-too-small
      after focuser-bounds clamp, sweep-completion + per-step
      `image_analysis` persistence anchor (5 FITS + 5 sidecars
      written; every sidecar carries `image_analysis`; no sidecar
      carries an aggregate `auto_focus` section). The
      "happy-path V-curve converges to known best_position" was
      deliberately omitted — OmniSim produces position-independent
      images, so a real V-curve never materializes in the simulator;
      numerical fit correctness is unit-tested instead.
- [x] `services/rp/src/imaging/tools/auto_focus.rs` — pure driver
      over `FocuserOps` / `CaptureOps` / `MeasureOps` traits so the
      loop and math are unit-testable without hardware. Validates
      params, builds the sweep grid (clamping out-of-range points
      rather than coercing — coercion would create duplicate samples
      at the bound), runs move/capture/measure for each grid point,
      fits a parabola via Cramer's rule on the 3×3 weighted
      least-squares system, validates that the fitted vertex falls
      inside the sampled grid (an `a > 0` parabola with vertex
      outside the grid is still monotonic over the sampled range),
      and moves the focuser to the fitted minimum on success. The
      MCP wrapper in `mcp.rs` is the thin adapter shell, sharing
      capture and move semantics with the primitive tools via two
      newly-extracted helpers (`do_capture`,
      `do_move_focuser_blocking`).
- [x] Unit tests on the parabola fit and the run loop. 17 unit
      tests cover validate_params (4), build_grid (5),
      fit_parabola (5 — known-vertex recovery to ±1 step,
      flat-input rejection, concave-down rejection,
      too-few-samples, zero-weight-sample skipping), and
      run_auto_focus end-to-end with synthetic adapters (3 —
      known-vertex recovery, grid-too-small-after-clamp,
      not-enough-stars-after-skips).

#### Phase 6c — `center_on_target`

Status: **complete (2026-05-09).** ADR-005 (plate solver: ASTAP via
subprocess + verification spike) landed on main on 2026-05-02,
retiring the prior blocker; the four sub-phases below shipped over
the following week.

`center_on_target` composes four primitive tools — `capture`,
`plate_solve`, `sync_mount`, `slew` — of which only `capture` existed
when this phase opened. Phase 6c therefore decomposed into four
sub-phases: the first two were prerequisites (mount primitives, the
plate-solver service); the third wired the plate solver into rp's
catalog; the fourth was the compound tool itself, mirroring Phase
6b's shape. Each sub-phase landed as its own PR.

##### Decisions resolved during Phase 6c planning

These were captured in `docs/services/rp.md` (Compound Tools →
`center_on_target` Contract — added by Phase 6c-3) and should not be
re-litigated:

- **`tolerance_arcsec` and `max_attempts`: required parameters, no
  defaults.** Same rationale as `measure_basic`'s `min_area` /
  `max_area`: the right values depend on rig pixel scale, mount
  tracking quality, and target altitude — none of which the tool can
  infer. Callers (workflows, plugins) own that policy.
- **`min_improvement_arcsec` deferred.** Useful future addition (early
  exit when an iteration stops improving the residual), but not v1.
  Add as an optional parameter when the first concrete need surfaces.
- **`sync_mount` only on the first iteration.** The first
  `plate_solve` produces the absolute pointing reference; subsequent
  iterations rely on the mount honouring relative `slew`s rather than
  re-syncing on each pass. Repeated syncs interact badly with
  model-building drivers (some treat each sync as a new pointing
  model entry, polluting the model) and are unnecessary once the
  absolute position is established.
- **Fail fast on per-iteration failures.** A `plate_solve`,
  `sync_mount`, `capture`, or `slew` failure inside the loop aborts
  `center_on_target` and propagates the error, with the mount left at
  its current (uncentered) position. No retry-with-longer-exposure
  fallback in v1 — workflows that want adaptive recovery wrap the
  call themselves. Captures already taken stay on disk normally;
  their exposure documents are intact.
- **No aggregate persistence section.** `center_on_target` does not
  write a `centering` section on any single document — same reasoning
  as `auto_focus`'s sweep-spanning result. Each per-iteration capture
  writes its own analysis sections (notably `wcs` from `plate_solve`)
  via the embedded calls. The compound result is returned in the MCP
  response and emitted as a `centering_complete` event.
- **Single `duration` parameter.** Required `humantime` string, same
  shape as `auto_focus`'s `duration`. No adaptive per-iteration
  exposure scaling in v1; if low star count blocks a solve, the
  caller re-runs with a longer duration.
- **Module home: `services/rp/src/imaging/tools/center_on_target.rs`.**
  Resolves the open question in the prior plan stub. Symmetry with
  `auto_focus.rs` outweighs the "it doesn't touch pixels" argument —
  every primitive `center_on_target` composes already lives in the
  imaging or MCP layer, and "compound tools live under
  `imaging/tools/`" is a clean rule.

##### Phase 6c-prep — Mount primitives

Status: **complete (2026-05-03).** Parallels Phase 6a (focuser
primitives). Required because `center_on_target` needs `slew` and
`sync_mount`, neither of which exists in `rp` today.

###### Decisions resolved during planning

These were captured in `docs/services/rp.md` (Configuration,
Built-in Tools — Hardware) and should not be re-litigated:

- **Singular `mount: Option<MountConfig>`, not plural `Vec`.** `rp`
  is not running farms of mounts. Piggyback rigs share **one** mount
  with multiple optical trains (multiple cameras, focusers, filter
  wheels) — those stay plural; the mount stays singular. The
  one-mount-per-deployment contract is expressed in the type, not
  enforced as a singleton runtime invariant. `Option` because a
  camera-only / flats-rig config remains valid.
- **Naming: "mount" in our code, "telescope" only at the Alpaca
  boundary.** `MountConfig`, `MountEntry`, `find_mount`, `mount.feature`,
  `slew`/`sync_mount` MCP tools. The `Arc<dyn Telescope>` field type
  and the `ascom-alpaca` `"telescope"` feature flag stay (forced by
  upstream). The two-name asymmetry matches our domain language ("the
  mount slewed to M31") rather than ASCOM's device-type label.
- **No `id` field on `MountConfig`.** Singular type, nothing to
  disambiguate. MCP tools take no `mount_id` / `telescope_id`
  parameter.
- **`slew` does not touch `Tracking`.** ASCOM mandates `Tracking ==
  true` before equatorial `SlewToCoordinatesAsync` (driver raises
  `InvalidOperationException` otherwise) and guarantees `Tracking ==
  true` after (per the AbortSlew docs: "equatorial slews by
  definition always start and finish with Tracking on"). The contract
  is unambiguous; `slew` propagates the natural Alpaca error if
  tracking is off rather than auto-enabling. No `ensure_tracking`
  parameter.
- **`set_tracking` and `get_tracking` are standalone MCP tools.**
  Workflows that need to enable tracking before slewing (the common
  case, after unparking) call `set_tracking` explicitly. `get_tracking`
  returns `{ tracking, can_set_tracking }`; fails loud when the
  underlying `Tracking` read errors.
- **`get_mount_position` is included** (un-deferred from the original
  stub). Returns `{ ra, dec }`. Needed for BDD verification that
  `sync_mount` took effect, and useful for any caller that wants to
  read the current pointing without going through `slew`.
- **`mount.settle_after_slew` is a config field with optional per-call
  override.** Mounts have post-slew mechanical settle (gear backlash,
  ringing) that's a property of the rig, not of any individual
  workflow. Operator sets `mount.settle_after_slew: "3s"` (or
  whatever their mount needs); `slew { ra, dec, settle_after?: "1s" }`
  overrides per call (including `"0s"` to skip). Default `"0s"` (no
  settle) so the field is purely opt-in.
- **300 s slew deadline, hardcoded.** Covers a worst-case
  meridian-flip + park-side traversal at ~1°/s. Move to config later
  if needed. Best-effort `abort_slew()` on deadline expiry before
  returning the timeout error (mount runaways are more dangerous than
  focuser runaways — cables, hard stops, sun-pointing in a flat
  workflow).
- **`get_focuser_position` follow-up: issue [#130](https://github.com/rusty-photon/rusty-photon/issues/130)**
  tracks revisiting `ensure_default_*` helpers in `bdd-infra` for
  piggyback (multi-optical-train) BDD scenarios. Out of scope for
  this PR.

###### Work breakdown (in order)

- [x] **Step 1 — `MountConfig` in `config.rs`.** Fields: `alpaca_url:
      String`, `device_number: u32` (`#[serde(default)]`), optional
      `auth: Option<ClientAuthConfig>`, optional
      `settle_after_slew: Option<Duration>` (`humantime_serde`).
      Replace the existing `pub mount: Value` placeholder on
      `EquipmentConfig` with `pub mount: Option<MountConfig>`
      (`#[serde(default)]`). Two unit tests — minimal-fields, with
      auth + settle_after_slew.
- [x] **Step 2 — `MountEntry` + `connect_mount` in `equipment.rs`.**
      Mirrors `FocuserEntry` / `connect_focuser`. Holds
      `Arc<dyn Telescope>` from ascom-alpaca. Five failure-arm unit
      tests via in-test axum stub (same pattern Phase 6a established
      for `connect_focuser`): `build_alpaca_client` error, `get_devices`
      network error, 5 s `tokio::time::timeout`, no-telescope-in-list,
      `set_connected` Alpaca rejection. Plus one happy-path arm. No
      connect-time probes of `Tracking` / `Slewing` / `AtPark`.
- [x] **Step 3 — `EquipmentRegistry` plumbing.** Singular
      `pub mount: Option<MountEntry>` field, populated in
      `EquipmentRegistry::new`. `find_mount(&self) -> Option<&MountEntry>`
      lookup. `EquipmentStatus.mount: Option<DeviceStatus>` (singular,
      JSON-serialized as `null` when no mount is configured). One
      end-to-end registry test against the axum stub paralleling
      `connect_focuser_success_returns_connected_entry` +
      `find_focuser`.
- [x] **Step 4 — `do_slew_blocking` helper in `mcp.rs`.**
      `pub(crate) async fn do_slew_blocking(&self, ra: f64, dec: f64,
      settle_after: Duration) -> Result<(f64, f64), String>`. Resolves
      the mount, calls `slew_to_coordinates_async`, polls `slewing()`
      every 100 ms with a 300 s deadline, sleeps `settle_after` after
      `Slewing == false`, then reads `(right_ascension(),
      declination())` and returns. Best-effort `abort_slew()` on
      deadline expiry before returning the timeout error. Modeled on
      `do_move_focuser_blocking`. Helper `resolve_mount` (no macro)
      handles "no mount configured" + "mount not connected" symmetrically.
- [x] **Step 5 — Five MCP tools in `mcp.rs`.**
      - `slew { ra: f64, dec: f64, settle_after: Option<Duration> }` →
        `{ actual_ra, actual_dec }`. Validates `ra ∈ [0, 24)` hours and
        `dec ∈ [-90, 90]` degrees in the body. `settle_after` resolves
        `None → config default`, `Some → override`.
      - `sync_mount { ra: f64, dec: f64 }` → `{}`. Same validation,
        calls `sync_to_coordinates`. Immediate, no polling.
      - `get_mount_position { }` → `{ ra: f64, dec: f64 }`. Reads
        `right_ascension()` then `declination()`. Fails loud on either
        read error.
      - `get_tracking { }` → `{ tracking: bool, can_set_tracking: bool }`.
        Reads both `tracking()` and `can_set_tracking()`. Fails loud
        on the `tracking()` read error (no half-success).
      - `set_tracking { enabled: bool }` → `{}`. Calls `set_tracking`.
        `CanSetTracking == false` surfaces as the natural Alpaca error.

      ~16 unit tests against a `MockTelescope` (paralleling the
      existing `MockFocuser` pattern at `mcp.rs:2160+`).
- [x] **Step 6 — `tests/features/mount.feature`** (~15 scenarios,
      `@serial` tagged). Catalog (1); `slew` (5: tracking-on happy
      path, tracking-off error, no-mount-configured, mount-not-connected,
      one Outline using `MISSING` sentinel for ra/dec range +
      missing-field cases); `sync_mount` (3: happy path verified via
      `get_mount_position`, no-mount/not-connected Outline,
      ra/dec range Outline); `get_tracking` (1); `set_tracking`
      (2: enable, disable); `get_mount_position` (2: happy path,
      no-mount/not-connected); plus one settle-honoring scenario
      asserting `settle_after: "100ms"` extends total slew time by
      ~100ms. Slew tolerance: exact equality (revisit at impl time
      if OmniSim diverges).
- [x] **Step 7 — `tests/bdd/steps/mount_steps.rs`.** Reuse-first:
      pull `the tool list should include`, `the tool call should
      succeed`, `the tool call should return an error`, `the error
      message should contain` from `tool_steps.rs`. New mount-specific
      steps: `Given(rp is running with a mount on the simulator)`,
      `Given(rp is running without a mount)`,
      `Given(rp is running with a mount at <url> device <n>)`,
      `Given(the mount tracking is set to <bool>)`, `When` steps for
      each MCP tool (slew/sync_mount/get_tracking/set_tracking/get_mount_position),
      `Then` field-extraction asserts. The slew/sync_mount `When`
      steps interpret `MISSING` in the table as "omit that field
      from the JSON-RPC params." No new `RpWorld` fields. Wire into
      `tests/bdd/steps/mod.rs`.
- [x] **Step 8 — `bdd_infra::rp_harness::MountConfig` +
      `RpConfigBuilder::with_mount`.** Test-side struct holds
      `alpaca_url` + `device_number` (no `auth`, matches existing
      test-side device structs). Builder field
      `mount: Option<MountConfig>`; `with_mount` setter (singular,
      not `add_*`). `build()` emits `"mount": null | { … }`,
      replacing the current literal `"mount": null` line. No
      `ensure_default_mount` — explicit `.with_mount(...)` reads
      better at every call site that needs a mount.
- [x] **Step 9 — `docs/services/rp.md` updates.**
      - Configuration example: replace the existing
        `mount` block with the singular typed shape including
        optional `settle_after_slew`. Drop the obsolete
        `settle_time_secs` field.
      - Built-in Tools — Hardware table: add three rows for
        `get_mount_position`, `get_tracking`, `set_tracking`.
      - One-line prose under Configuration: "Exactly one mount is
        the typical deployment — the singular `mount` field reflects
        that. Piggyback rigs have multiple cameras / focusers /
        filter wheels on the same mount; multi-mount support is in
        Future Considerations."
      - One-line prose under the new tools: "`mount.settle_after_slew`
        is applied after `Slewing == false`; per-call `settle_after`
        on `slew` overrides (including `"0s"` to skip)."
- [x] **Step 10 — `services/rp/Cargo.toml`.** Append `"telescope"` to
      `ascom-alpaca`'s features list. No new workspace deps; no
      `bazel mod tidy` needed. Per-crate features lists are intent
      expression — workspace builds compile the union due to Cargo
      feature unification, but cargo-hack's feature-powerset CI
      enforces standalone-buildability.
- [x] **Step 11 — Plan-doc bookkeeping.** Land alongside the impl:
      flip Phase 6c-prep status to **complete** with the work-breakdown
      checkboxes ticked, exit-criteria block populated with concrete
      counts (BDD scenarios green, lib tests passing).

**Out of scope (deferred or never):**

- `set_park` MCP tool (sets the current position as the new park
  position). Power-user feature with no workflow consumer in sight;
  defer until one surfaces.
- `tracking_rate` (sidereal / lunar / solar / king). Sidereal is the
  only rate deep-sky workflows need; defer until a planetary or
  comet-tracking workflow surfaces.
- Multi-mount support. Already in `rp.md` Future Considerations.
- `side_of_pier` exposure. The original stub mentioned it on
  `get_mount_position`; deferred until a meridian-flip workflow
  surfaces.

**Follow-up landed:** `park`, `unpark`, `get_park_state`, and
`abort_slew` MCP tools landed in the next PR after Phase 6c-prep.
The previous "deferred" entries for these tools have been removed.

**Exit criteria met:** 14 new BDD scenarios green (187/187 total in
rp's BDD suite, was 173/173 pre-Phase-6c-prep); 33 new lib tests
across `config` (3), `equipment` (7 — 5 `connect_mount` failure paths
+ 1 success arm + 1 `EquipmentRegistry` end-to-end via axum stub),
and `mcp` mount tools (23 against `MockTelescope`); plus 3 new
`bdd-infra` tests pinning `RpConfigBuilder::with_mount` JSON shape.
`cargo rail run --profile commit -q` reports 701/701 tests passing
workspace-wide. `cargo fmt` clean. No new workspace deps; no
`bazel mod tidy` needed. Slew-echo BDD assertion uses a
`0.001`-hour / `0.001`-degree tolerance because OmniSim's slew echo
drifts ~0.4 arcsec from internal coordinate transforms — well under
any centering workflow's tolerance, well above OmniSim's drift.

##### Phase 6c-1 — `plate-solver` rp-managed service

Status: **complete.** Sequenced separately from this plan because of
its size (subprocess supervision, ASTAP CLI argument layout, `.wcs`
parsing, per-platform install verification) and because ADR-005
already called for "a separate plan to sequence the plate-solver
rp-managed-service implementation." Tracked end-to-end in
[`docs/plans/archive/plate-solver.md`](plate-solver.md), where all eight
phases (Phase 1 design doc through Phase 8 LGPL §4/§6 review) landed.

- [x] **`docs/plans/archive/plate-solver.md`** sequenced the service itself:
      workspace member skeleton, ASTAP subprocess wrapper, HTTP API,
      BDD scenarios, supervision integration with sentinel, and the
      per-platform end-to-end solve passes that retired ADR-005 Open
      Questions 1–6.
- [x] **HTTP contract (frozen here so 6c-2 could mock it):**
      `POST /api/v1/solve` with body
      `{fits_path, ra_hint?, dec_hint?, fov_hint_deg?, search_radius_deg?, timeout?}`
      returns `{ra_center, dec_center, pixel_scale_arcsec, rotation_deg}`
      on success, structured error otherwise. Path-based input matches
      ADR-005's "rp and plate solver share a filesystem" contract — no
      pixel bytes over HTTP.
- [x] **Service supervision** via the existing rp-managed-service
      pattern, mirroring `services/phd2-guider` and `services/sentinel`
      crate layout, lifecycle, and sentinel integration.

This sub-phase was the largest in 6c by far. It was independent of
6c-prep and 6c-3 once the HTTP contract above was fixed; the in-test
stub plate-solver in 6c-2 satisfied that phase until the real service
landed.

##### Phase 6c-2 — `plate_solve` built-in MCP tool

Status: **complete (2026-05-05).** All four steps landed; 235/235
BDD scenarios green (was 216/216 pre-Phase-6c-2 + 19 new), 879
tests pass workspace-wide via `cargo rail run --profile commit -q`.
New crate `crates/rp-plate-solver` carries the HTTP client +
mockall mock; `services/rp` adds the `plate_solver` config block,
the `plate_solve` MCP tool, and the `resolve_document_by_path`
helper for the late-solve workflow's image_path persistence.

###### Decisions resolved during Phase 6c-2 planning

These were captured in `docs/services/rp.md` (Built-in Tools — Image
Analysis; Configuration; new `plate_solve` Contract subsection added by
Step 1) and should not be re-litigated:

- **Persistence section name: `wcs`.** Payload mirrors the wrapper's
  response field-for-field: `{ra_center, dec_center,
  pixel_scale_arcsec, rotation_deg, solver}`. Matches what the result
  *is* (a WCS solution), not what produced it. Distinct from the
  existing `image_analysis`, `background`, `detected_stars`,
  `measured_stars`, `snr` sections so all tools coexist on one
  document. The historical `plate_solve` section name in `rp.md`
  Document Model (line ~178) is updated to `wcs` in Step 1.
- **`document_id` and `image_path`: not strict xor.** Mirrors the
  existing imaging-tool convention (`rp.md` line 645–646; confirmed in
  `mcp.rs:1422` for `measure_basic`): both fields optional at the
  serde level, at least one required, **`document_id` takes
  precedence when both supplied**. Persistence behavior:
  - `document_id` mode: persist `wcs` section to that document via
    `ImageCache::put_section`.
  - `image_path` mode: solve runs; on success, derive the sibling
    sidecar path (`<base>.fits` → `<base>.json`) and resolve via
    the new `ImageCache::resolve_document_by_path`. Match found
    (rp-produced FITS with sidecar on disk — the late-solve
    workflow's case) ⇒ persist `wcs` to its sidecar. No match
    (external FITS, missing sidecar, non-`.fits` path) ⇒ return
    result without persistence and `debug!()` log it. The
    late-solve workflow (capture frame N → start capture N+1 →
    solve frame N → update the original sidecar) falls in the
    first bucket. `put_section` itself falls back to a disk-only
    write when the cache entry is absent (post-eviction or
    post-`rp` restart) so persistence is durable across cache
    misses; the original UUID-8 reverse-lookup proposal in the
    plan-stub was superseded during implementation by this cleaner
    sibling-sidecar derivation.
- **Hints: explicit by default, `use_mount_hints` is the opt-in
  convenience.** No auto-sourcing from the mount in the default path
  (per the operator preference: explicit beats auto-magic). MCP shape:
  - `pointing_hint: { ra_deg: f64, dec_deg: f64 }` — nested object so
    the both-or-neither contract is structural, not runtime-validated.
    Field names carry units to preempt the Alpaca-hours / wrapper-
    degrees gotcha at the call site.
  - `use_mount_hints: bool` — convenience flag that reads the current
    mount position (`right_ascension()` × 15 → degrees, `declination()`
    pass-through) and forwards as the wrapper's `ra_hint` / `dec_hint`.
  - Mutually exclusive: `pointing_hint` + `use_mount_hints == true` ⇒
    error (`provide explicit pointing_hint or use_mount_hints, not
    both`).
  - `use_mount_hints == true` with no mount configured / not connected
    / Alpaca read failure ⇒ error to caller (caller asked, fail loud).
  - Both absent ⇒ wrapper falls back to blind solve.
  - rp handler maps `pointing_hint.ra_deg` → wrapper's `ra_hint`,
    `pointing_hint.dec_deg` → wrapper's `dec_hint`. The hours→degrees
    conversion lives in the rp handler (mount Alpaca returns hours;
    wrapper takes degrees per `plate-solver.md` §"Hint Mapping").
  - `center_on_target` (Phase 6c-3) sets `use_mount_hints: true`
    rather than calling a Rust-side mount-read helper.
- **`fov_hint_deg`: optional MCP parameter, no per-camera config in
  v1.** Same shape as the rest of the optional hints. Per-camera FOV
  stash (the eventually-right home for this value) is tracked by
  [issue #153](https://github.com/rusty-photon/rusty-photon/issues/153)
  — defer until a workflow is blocked without it.
- **`search_radius_deg`: optional MCP parameter overrides
  `plate_solver.default_search_radius_deg` config.** Operator-set
  default in config (no default — operator opt-in) for fresh captures
  on the configured rig; per-call MCP parameter overrides for
  loaded-from-disk images where the configured default may not match.
  Both absent ⇒ omit from wrapper request ⇒ ASTAP uses its own
  default.
- **Two timeouts.** Solve-side: per-call MCP `timeout` (humantime
  string) forwarded to wrapper's `timeout` field via
  `humantime_serde::option`. Omitted ⇒ wrapper applies its own
  `default_solve_timeout`. Connection-side: `plate_solver.timeout:
  Duration` (`humantime_serde`, default `"60s"`) is the rp HTTP-client
  outer timeout — required-with-default per Tenet 1; the backstop is
  non-optional. Maps to `reqwest::ClientBuilder::timeout()` at client
  build time so it covers connection + total request.
- **`pub plate_solver: Option<PlateSolverConfig>` on top-level
  `Config`.** Mirrors `mount: Option<MountConfig>` (Phase 6c-prep):
  the service is optional infrastructure; configs without it remain
  valid. Absent ⇒ `plate_solve` tool returns `plate_solve: plate
  solver not configured`. `#[serde(default)]` so existing rp configs
  load.
- **HTTP client: `reqwest`.** Already a workspace dep used heavily by
  rp (`events.rs` webhooks, `equipment.rs` Alpaca clients,
  `session.rs`). One client built once at AppState construction from
  `PlateSolverConfig`, held as `Option<PlateSolverClient>` inside
  `AppState`. Connection pooling matters even for a single endpoint.
- **Error mapping at the MCP boundary: plain string `plate_solve:
  <code>: <message>`.** Symmetric with existing imaging-tool errors
  (no structured code field). Propagate all wrapper errors verbatim
  (`invalid_request`, `fits_not_found`, `solve_failed`,
  `solve_timeout`, `internal`); wrapper's `details` (when present, for
  `solve_failed`) appended. Connection-level failures surface as
  `plate_solve: service unreachable: <reason>` so they're
  distinguishable from solver-side failures. No defensive
  pre-validation in rp — wrapper is authoritative on `fits_path`.
- **Module location: new `crates/rp-plate-solver` workspace member.**
  Matches the established `crates/rp-*` convention (`rp-auth`,
  `rp-catalog`, `rp-ephemeris`, `rp-fits`, `rp-tls` — "shared
  utilities for Rusty Photon services"). Forward-positions for
  `rp-guider` and any future rp-managed-service client. Single-
  direction client — `services/plate-solver` does **not** depend on
  it; Tenet 4 (remote interfaces only) preserved by HTTP staying as
  the only inter-service contract. Crate exports a `PlateSolveClient`
  trait + `MockPlateSolveClient` (mockall) so the MCP handler can be
  unit-tested without standing up an axum stub — same shape
  `auto_focus.rs` uses with its `FocuserOps`/`CaptureOps`/`MeasureOps`
  trait adapters.
- **No new feature gates.** `rp-plate-solver` is an unconditional dep
  of `services/rp`; the `Option<PlateSolverConfig>` is the only knob
  — present means available, absent means tool returns "not
  configured."

###### Work breakdown (in order)

**Step 1 — Design doc updates (Phase 1 of dev workflow).**

- [x] `docs/services/rp.md` updates:
   - Built-in Tools — Image Analysis table: add `plate_solve` row
     with the final input shape (`document_id` | `image_path`,
     optional `pointing_hint` / `use_mount_hints` / `fov_hint_deg` /
     `search_radius_deg` / `timeout`); returns `ra_center`,
     `dec_center`, `pixel_scale_arcsec`, `rotation_deg`, `solver`.
   - New `plate_solve` Contract subsection (parallel to
     `measure_basic` Contract): inputs, hint shape with the
     mutually-exclusive validation rules and the hours→degrees
     conversion called out, error-code propagation, persistence
     rules for `document_id` mode and `image_path`-with-UUID-8-match
     mode.
   - Configuration block (already updated for `timeout: "60s"`):
     extend with `default_search_radius_deg` (optional).
   - Document Model example (line ~178): rename `plate_solve`
     section to `wcs`, update field names to wrapper's response
     shape (`ra_center`, `dec_center`, `pixel_scale_arcsec`,
     `rotation_deg`, `solver`).
- [x] `docs/services/plate-solver.md` §"Hint sources and
  search-radius defaults": flip the auto-sourcing description ("rp's
  `plate_solve` MCP handler reads both via the equipment registry,
  performs the RA conversion, and forwards the request") to "rp's
  `plate_solve` MCP handler accepts explicit hints from the caller;
  the `use_mount_hints` convenience opts into reading the current
  mount position (with hours→degrees conversion)."

**Step 2 — BDD feature + step defs (Phase 2 of dev workflow, with
`@wip`).**

- [x] `services/rp/tests/features/plate_solve.feature` (~17
  scenarios, `@wip` tag at top per `testing.md` §2.6):
  1. Catalog includes `plate_solve` (1)
  2. Happy path — `image_path` mode against in-test stub returns
     expected fields (1)
  3. Happy path — `document_id` mode resolves to file_path via
     cache, returns same shape (1)
  4. `wcs` section persistence anchor — `document_id` mode (1)
  5. `wcs` section persistence — `image_path` mode against an
     rp-produced FITS (UUID-8 suffix matches a known document) (1)
  6. `image_path` mode against an external FITS (no UUID-8 match) ⇒
     no persistence, result returned (1)
  7. `document_id` precedence — both supplied, document_id wins (1)
  8. Explicit `pointing_hint` forwarded verbatim (stub request log
     pinned) (1)
  9. `use_mount_hints: true` reads mount, forwards converted RA
     (×15 → degrees) and Dec (1)
  10. `use_mount_hints: true` with no mount configured ⇒ error (1)
  11. `use_mount_hints: true` with mount disconnected ⇒ error (1)
  12. `pointing_hint` + `use_mount_hints: true` ⇒ error (1)
  13. No hints ⇒ blind solve (request omits hint fields) (1)
  14. Per-call `fov_hint_deg` / `search_radius_deg` / `timeout`
      forwarded verbatim (1)
  15. Config `default_search_radius_deg` applied when MCP parameter
      omitted; per-call value overrides (1)
  16. `plate solver not configured` error when config absent (1)
  17. `service unreachable` error — config points at unbound port (1)
  18. Wrapper error round-trip — stub returns each of the 5 error
      codes (Scenario Outline) (1)
  19. Missing-parameter — neither `document_id` nor `image_path` (1)

  (Final count likely 18–19 with the Scenario Outline; tighten in
  Step 2 implementation.)
- [x] `services/rp/tests/bdd/steps/plate_solve_steps.rs` reusing
  `tool_steps.rs` shared steps. New steps:
  - `Given(a stub plate solver returning a canned WCS)` — spawns
    axum router on `127.0.0.1:0`, holds receiver-side request log
    so subsequent steps can assert hint plumbing.
  - `Given(a stub plate solver returning <error_code>)` — variant
    returning a structured ErrorResponse envelope.
  - `Given(rp is configured to point at a port with no plate
    solver)` — sets `plate_solver.url` to an unbound port for the
    unreachable scenario.
  - `When(I call plate_solve with <table>)` — JSON-RPC call; table
    interprets `MISSING` sentinel as omitted field.
  - `Then(the plate solver received hints ra=<f> dec=<f>)` and the
    negative variant.
- [x] `RpWorld` additions: `last_plate_solver_stub:
  Option<PlateSolverStub>` (handle wrapping the stub server for
  inspection), `last_plate_solve_result: Option<Value>`. Wire into
  `tests/bdd/steps/mod.rs`.
- [x] `bdd_infra::rp_harness::RpConfigBuilder::with_plate_solver(url,
  timeout, default_search_radius_deg: Option<f64>)` builder method,
  mirroring `with_mount`. Builder emits `"plate_solver": null | { …
  }` so existing scenarios that don't set it stay unaffected.

**Step 3 — Implementation (Phase 3 of dev workflow).**

- [x] `crates/rp-plate-solver/` (new workspace member):
  - `Cargo.toml` mirroring the other `rp-*` crates' shape.
    Workspace deps only (`reqwest`, `serde`, `serde_json`,
    `thiserror`, `humantime-serde`, `tracing`, `mockall` for tests).
  - `BUILD.bazel`. `CARGO_BAZEL_REPIN=1 bazel mod tidy` if any new
    crates.io deps land — none expected.
  - `lib.rs` — public API: `PlateSolveClient` trait, `SolveRequest`,
    `SolveOutcome`, `SolveError`. Concrete `PlateSolverClient`
    implementing the trait via `reqwest::Client`.
    `MockPlateSolveClient` via `#[cfg_attr(test, mockall::automock)]`.
  - Local types do **not** depend on the `plate-solver` crate (Tenet
    4). Wire types match the wrapper's HTTP contract verbatim.
  - Unit tests via in-test axum stub (same pattern as
    `equipment.rs:connect_focuser` tests at line 791+): happy path,
    each of the 5 wrapper error codes round-trip, connection-refused,
    body-parse-failure (200 with malformed JSON ⇒ `Internal`).
- [x] `services/rp/Cargo.toml`: add `rp-plate-solver = { workspace =
  true }` dep. Workspace `Cargo.toml`: register the new crate as a
  workspace member and dep.
- [x] `services/rp/src/config.rs`:
  - New `PlateSolverConfig { url: String, timeout: Duration
    (humantime, default `"60s"`), default_search_radius_deg:
    Option<f64> }`.
  - `pub plate_solver: Option<PlateSolverConfig>` on top-level
    `Config` with `#[serde(default)]`.
  - 2 unit tests: minimal-fields (just `url`, defaults applied),
    with-default-search-radius.
- [x] `services/rp/src/mcp.rs`:
  - New `PlateSolveParams { document_id, image_path, pointing_hint:
    Option<PointingHint>, use_mount_hints: Option<bool>,
    fov_hint_deg, search_radius_deg, timeout }` struct (all optional
    at serde for the same error-message-ordering reason as
    `MeasureBasicParams`). New nested `PointingHint { ra_deg: f64,
    dec_deg: f64 }`.
  - New `#[tool] async fn plate_solve(...)` handler:
    1. Validate `document_id` xor `image_path` (matching existing
       imaging-tools error message shape mentioning `image_path`).
    2. Validate hint mutual-exclusion + `use_mount_hints` ⇒
       connected mount.
    3. Resolve plate solver client (`tool_error!("plate_solve: plate
       solver not configured")` if absent on `AppState`).
    4. Resolve `fits_path`: `document_id` mode goes through
       `image_cache.resolve_document(doc_id)` then reads
       `doc.file_path`; `image_path` mode forwards verbatim.
    5. Resolve hints: explicit `pointing_hint` → wrapper's flat
       fields; `use_mount_hints` → mount read + ×15 RA conversion.
    6. Resolve `search_radius_deg`: per-call value > config default >
       `None`.
    7. Build `SolveRequest`, call client, map `SolveError` → MCP
       error per Decision 9.
    8. Persistence: `document_id` mode → `image_cache.put_section
       (doc_id, "wcs", payload)`. `image_path` mode → attempt UUID-8
       reverse-lookup; persist if matched, debug-log if not.
       Failure of persistence logged at `debug!()`, does not fail
       the tool (matches imaging-tool convention).
  - ~14 unit tests against `MockPlateSolveClient` (paralleling the
    existing `MockTelescope` and `MockFocuser` patterns): happy
    path, hint validation arms, document_id resolution,
    `image_path`-with/without-UUID-match persistence, each wrapper
    error propagated, missing-param errors.
- [x] `services/rp/src/lib.rs` / `main.rs`: wire the new
  `PlateSolverConfig` into a `Option<Box<dyn PlateSolveClient>>`
  field on `AppState` (trait-typed so tests can swap in the mock).

**Step 4 — Activate BDD + cleanup.**

- [x] Remove `@wip` tag from `plate_solve.feature` line 1.
- [x] Run full suite (`cargo rail run --profile commit -q` per
  CLAUDE.md rule 4); confirm BDD count: 187 + ~18 = ~205 scenarios
  green.
- [x] `cargo fmt`. `cargo build --all --all-targets --all-features
  --locked` for linker-visible code per CLAUDE.md rule 4.
  `CARGO_BAZEL_REPIN=1 bazel mod tidy` only if new crates.io deps
  landed (none expected).
- [x] Plan-doc bookkeeping: flip Phase 6c-2 status to **complete**
  with the work-breakdown checkboxes ticked, exit-criteria block
  populated with concrete BDD/unit-test counts.

###### Out of scope / deferred

- **Per-camera FOV stash.** Tracked by issue #153.
- **Explicit `pointing_hint` *plus* mount-fallback.** v1 is "explicit
  or mount, not both." A blended mode (`pointing_hint` overrides what
  `use_mount_hints` would have pulled, missing fields fall through
  to mount) is the natural extension when a caller needs it.
- **Background solve via `exposure_complete` subscription.** Already
  deferred in `plate-solver.md` MVP scope.
- **Solve cache / warm process pool.** Wrapper is stateless-across-
  requests by design.
- **Multi-target solve / batch endpoint.** Wrapper exposes a
  single-flight endpoint; no batching contract to wrap.

###### Exit criteria

- ~18 new BDD scenarios green (~205/205 total in rp's BDD suite, was
  187/187).
- ~14 new lib tests across `services/rp/src/config` (2),
  `services/rp/src/mcp::plate_solve` (~12); plus ~9 unit tests in the
  new `crates/rp-plate-solver` crate (happy path + 5 wrapper error
  arms + connection-refused + body-parse-failure + mockall
  scaffolding test).
- `cargo rail run --profile commit -q` clean workspace-wide,
  `cargo fmt` clean. New workspace member registered; no new
  crates.io deps expected (so no `bazel mod tidy` required —
  confirm before committing).
- `wcs` section round-trips through `GET /api/documents/{id}` for
  both `document_id` and `image_path`-with-UUID-match modes.
- Phase 6c-3 (`center_on_target`) is now unblocked — its
  `PlateSolveOps` adapter wraps `PlateSolveClient` directly and its
  hint plumbing sets `use_mount_hints: true`.

##### Phase 6c-3 — `center_on_target` design + BDD + impl

Status: **complete (2026-05-09).** All four steps landed; 261/261
BDD scenarios green (was 238/238 pre-Phase-6c-3 + 14 new). The
new `center_on_target` MCP tool composes capture / plate_solve /
sync_mount / slew per its Contract in `docs/services/rp.md`; the
pure driver in `services/rp/src/imaging/tools/center_on_target.rs`
is unit-tested against synthetic adapters (sync-on-iter-1-only
invariant verified by call-count on a `StubMounter`).
Mirrors Phase 6b's shape (design contract → BDD → pure-Rust driver
behind trait adapters → MCP wrapper) and Phase 6c-2's plan layout
(decisions-resolved-first, then a work-breakdown ordered Step 1
design doc → Step 2 BDD with `@wip` → Step 3 implementation →
Step 4 activation). Per the
[development-workflow](../../skills/development-workflow.md) procedure,
no implementation landed before BDD scenarios existed on disk.

###### Decisions resolved during Phase 6c-3 planning

These were captured in `docs/services/rp.md` (Compound Tools →
new `center_on_target` Contract subsection added by Step 1) and should
not be re-litigated. The first three were already pinned in the prior
Phase 6c-3 stub and the parent decision block above
(line 510 onward); the remainder resolve open questions surfaced
during this planning pass.

- **`tolerance_arcsec` and `max_attempts`: required parameters, no
  defaults.** (Carried from prior planning.) Same rationale as
  `measure_basic`'s `min_area`/`max_area`: the right values depend
  on rig pixel scale, mount tracking quality, and target altitude.
- **`sync_mount` only on the first iteration.** (Carried.) The first
  `plate_solve` produces the absolute pointing reference; subsequent
  iterations rely on the mount honouring relative `slew`s. Sync
  *unconditionally* on iter 1 — even if the residual is already
  inside tolerance — so the mount's pointing model is calibrated for
  any caller that follows centering with further targeted slews.
- **Fail fast on per-iteration failures.** (Carried.) A `capture`,
  `plate_solve`, `sync_mount`, or `slew` error inside the loop aborts
  with the underlying message; the mount is left where the failed
  step left it. No retry-with-longer-exposure fallback in v1.
- **No aggregate persistence section.** (Carried.) Each per-iteration
  capture writes its own `wcs` section via the embedded
  `plate_solve`. The compound result is returned in the MCP response
  and emitted as `centering_complete`. A `centering_started` event
  fires before the loop and a `centering_iteration` event fires
  after each per-iteration solve so a live UI can stream the
  residual without polling the document store.
- **Module home:
  `services/rp/src/imaging/tools/center_on_target.rs`.** (Carried.)
  Symmetry with `auto_focus.rs`.
- **No `telescope_id` parameter.** Mount is singular per Phase 6c-prep
  (`mount: Option<MountConfig>`, no `id` field). The driver and the
  MCP shape both resolve the mount via the singleton — a `mount_id`
  parameter would be dead code. The original Phase 6c-3 stub's
  `telescope_id` is stale from before 6c-prep and is dropped here.
- **No `min_area`/`max_area`/`threshold_sigma` parameters.** The loop
  composes `capture` + `plate_solve` + `sync_mount` + `slew`; none
  takes star-detection thresholds (ASTAP owns its own internal
  detection). The original stub's inclusion of these is stale from
  the early draft when `measure_basic` was envisioned in the loop.
  Dropped.
- **Hint plumbing: `use_mount_hints: true` on every inner
  `plate_solve` call.** (Already pinned in 6c-2 decision block
  line 857.) The first iteration solves blind-but-with-mount-hints
  (mount has just been commanded to `(ra, dec)` by the caller's
  pre-flight slew, or — more often — left at its last position from
  a prior workflow step). After the first sync, the mount's reported
  position tracks reality, so the hint stays accurate across
  iterations. Sourcing the hint *via* `plate_solve`'s existing
  `use_mount_hints` flag rather than a Rust-side mount-read keeps
  the hours→degrees conversion in exactly one place.
- **Settle handling: per-iteration `slew` honours `mount.settle_after_slew`
  config.** No per-call `settle_after` override in v1 — the rig's
  configured settle is the right value across iterations, and an
  override would only matter if we wanted iteration N+1's settle to
  differ from N's, which there's no use case for. Pass-through to
  `do_slew_blocking` with `settle_after = config_default`.
- **Always-sync-on-iter-1 *then* check residual *then* slew if
  outside tolerance.** Algorithm clarification on the "single-
  iteration happy path" case from the prior stub: even when the open-
  loop pointing was already accurate enough to land inside tolerance
  on the first solve, we still issue `sync_mount` (per the bullet
  above) — but we do **not** issue a `slew` if the residual is
  already inside tolerance. The mount is at the right place; we just
  taught it where it is. Skip the redundant slew.
- **`max_attempts` safety cap: hardcoded `MAX_ATTEMPTS = 50`.**
  Mirrors `auto_focus`'s `MAX_GRID_POINTS` guardrail. Plausible
  centering runs converge in 2–4 iterations; 50 is generous enough
  that no real workflow trips it, low enough that a misconfigured
  loop can't tie up the rig for hours.
- **Output shape:
  `{final_error_arcsec, attempts, final_ra, final_dec, iterations}`.**
  `iterations[i]` is `{document_id, residual_arcsec, solved_ra,
  solved_dec, action}` where `action ∈ {"sync", "slew",
  "converged"}` records what happened *after* the per-iteration
  solve — `"sync"` on iter 1 followed by either `"slew"` or
  `"converged"` depending on whether the residual was inside
  tolerance, `"slew"` or `"converged"` on subsequent iterations.
  Matches `auto_focus`'s per-step `curve_points` provenance shape
  so callers can fetch the per-iteration documents (which carry the
  `wcs` section written by the embedded `plate_solve`).
- **Great-circle residual via the haversine formula.** Standard
  closed-form equation; numerically stable for the small angles
  centering deals with (typically arcseconds-to-arcminutes). No
  external dep needed. Inputs from `plate_solve` are decimal degrees
  for both RA and Dec; the formula returns arcseconds directly.
- **`PlateSolveOps` and `MountOps` driver traits added in
  `imaging/tools/center_on_target.rs`.** Mirror `auto_focus`'s
  `FocuserOps`/`CaptureOps`/`MeasureOps` shape — async traits,
  `String` errors, single-method per role. Reuse the existing
  `CaptureOps` trait from `auto_focus.rs` rather than redefining
  it (a `capture(duration) → document_id` operation is identical
  in both compound tools); move the trait to a shared module
  (`imaging/tools/mod.rs` re-export, or `imaging/tools/ops.rs`)
  if and only if the second consumer arrives — it has, so the move
  lands in this phase.
- **MCP-side helpers reused, not re-extracted.** `do_capture`,
  `do_slew_blocking`, and `read_mount_hints_for_plate_solve` already
  exist (`services/rp/src/mcp/internals.rs:408,721,812`). The plan
  stub's call for "newly-extracted `do_slew_blocking` / `do_sync_mount`
  helpers" is partly stale — `do_slew_blocking` is already a helper.
  This phase adds a `do_sync_mount(ra, dec)` helper for symmetry and
  to give the adapter a single place to call for sync semantics.
  Inner `plate_solve` calls go through the in-process
  `McpHandler::plate_solve` rather than the HTTP MCP transport (no
  re-encoding round-trip — same shape `auto_focus` uses for its
  inner `measure_basic` calls via `measure_via_document`).

###### Work breakdown (in order)

**Step 1 — Design doc updates (Phase 1 of dev workflow).**

- [x] `docs/services/rp.md` updates:
  - Built-in Tools — Compound table (line ~723): replace the existing
    `center_on_target` placeholder row with the final input/output
    shape from the decisions above (drop `tolerance_arcsec` placeholder
    expansion to: `camera_id, ra, dec, duration, tolerance_arcsec,
    max_attempts`; returns `final_error_arcsec, attempts, final_ra,
    final_dec, iterations`). Flip the `*Planned.*` tag to
    `Implemented.` only after Step 4 activation.
  - New `center_on_target` Contract subsection (parallel shape to
    `auto_focus` Contract at line ~1832 and `plate_solve` Contract at
    line ~2026): Input, Output, Algorithm, Error cases, Persistence,
    Caveats. Algorithm captures the sync-on-iter-1 invariant, the
    residual-then-slew sequence, the haversine residual formula, and
    the events emitted at each loop boundary.
  - Update the existing `center_on_target` pseudo-code example (line
    ~2183) to match the v1 input/output shape (drop the implicit
    `min_area`/`max_area`, add `max_attempts`, surface the
    per-iteration `iterations[]` provenance).

**Step 2 — BDD feature + step defs (Phase 2 of dev workflow, with
`@wip`).**

- [x] `services/rp/tests/features/center_on_target.feature`
  (~14 scenarios, `@wip` tag at top per
  [testing.md §2.7](../../skills/testing.md#27-use-wip-tag-for-scenarios-without-implementation-yet)):
  1. Catalog includes `center_on_target` (1)
  2. Single-iteration happy path — first solve already inside
     tolerance, sync fires, no slew, returns `attempts=1`,
     `iterations[0].action="converged"` (1)
  3. Multi-iteration happy path — residual outside tolerance on
     iter 1 then inside on iter 2; sync-on-iter-1 verified via the
     stub plate solver's call log + `iterations[]` shape (1)
  4. `tolerance_not_reached` error after `max_attempts` exhausted —
     stub returns the same large residual every iteration (1)
  5. Mid-loop `plate_solve` failure aborts with the wrapper's error
     code propagated; mount left at its last commanded position (1)
  6. Mid-loop `slew` failure (e.g. tracking off after iter 1)
     aborts with the underlying mount error (1)
  7. Sync-on-first-only invariant: 3-iteration run logs exactly one
     `sync_to_coordinates` call against the OmniSim mount (1)
  8. Per-iteration `wcs` section persists on every captured
     document — fetch each `iterations[i].document_id` and verify
     the section is present (1)
  9. Catalog row: nonexistent camera (1)
  10. Catalog row: disconnected camera (1)
  11. Catalog row: no mount configured (1)
  12. Catalog row: mount not connected (1)
  13. Scenario Outline: missing required parameters (`camera_id`,
      `ra`, `dec`, `duration`, `tolerance_arcsec`, `max_attempts`)
      — error message names the missing field, validated in input
      order (1, six examples)
  14. Scenario Outline: numeric-range rejections — `tolerance_arcsec
      <= 0`, `max_attempts <= 0`, `max_attempts > MAX_ATTEMPTS`
      (50), `ra` outside `[0, 24)`, `dec` outside `[-90, 90]`
      (1, five examples)

  OmniSim caveat from Phase 6b applies — OmniSim does not model
  pointing, so happy-path *convergence* (residual decreasing across
  iterations) is asserted via the stub plate solver's canned per-
  iteration responses, not via a real solve cycle. The end-to-end
  pixel→solve→sync→slew loop still exercises the live OmniSim
  camera and mount; only the solver is stubbed.
- [x] `services/rp/tests/bdd/steps/center_on_target_steps.rs`
  reusing `tool_steps.rs` shared steps. New steps:
  - `Given(a stub plate solver returning per-iteration WCS responses)`
    — extends the existing `PlateSolverStub` from
    `plate_solve_steps.rs` with a queue of `SolveOutcome` values
    consumed in order (one per `solve` call).
  - `When(I call center_on_target with camera <camera> ra <ra>
    dec <dec> duration <dur> tolerance_arcsec <tol> max_attempts <n>)`
    — JSON-RPC call.
  - `When(I call center_on_target omitting "<missing_param>")`
    — Outline-driven missing-param probe; reuses the
    `MISSING`-sentinel pattern from `mount_steps.rs`.
  - `Then(the stub plate solver should have received <n> solve
    calls)` — counts entries in the receiver-side request log.
  - `Then(the OmniSim mount should have received exactly one sync
    call)` — reads the OmniSim `telescope/{n}/synctocoordinates`
    counter (matches the request-log shape Phase 6c-prep
    introduced for the `sync_mount`-honoring scenario).
  - `Then(the centering result should report attempts <n>)`,
    `Then(the centering result iterations[<i>].action should be
    "<action>")`, `Then(every iterations[i].document_id sidecar
    should contain a "wcs" section)`.
- [x] `RpWorld` additions: `last_center_on_target_result:
  Option<Value>`. Reuse the existing `last_plate_solver_stub` from
  `plate_solve_steps.rs`.
- [x] No new `RpConfigBuilder` methods — `with_plate_solver` and
  `with_mount` already exist from Phases 6c-2 and 6c-prep.
- [x] Wire into `tests/bdd/steps/mod.rs`.

**Step 3 — Implementation (Phase 3 of dev workflow).**

- [x] `services/rp/src/imaging/tools/center_on_target.rs`:
  - `CenterOnTargetParams { ra, dec, duration, tolerance_arcsec,
    max_attempts }`. `CenterOnTargetResult { final_error_arcsec,
    attempts, final_ra, final_dec, iterations: Vec<IterationRecord> }`.
    `IterationRecord { document_id, residual_arcsec, solved_ra,
    solved_dec, action: IterationAction }`. `IterationAction` is
    a `Serialize` enum with three string variants `"sync"`, `"slew"`,
    `"converged"` (NB: iter 1 records the `"sync"` action *before*
    the residual check, then iter 1's converged-or-slew action is
    recorded on the same `IterationRecord`; the JSON-shape pin
    happens in Step 1's design doc).
  - `CenterOnTargetError { InvalidTolerance(f64), InvalidMaxAttempts(usize),
    MaxAttemptsExceedsCap { requested: usize, max: usize },
    ToleranceNotReached { last_residual_arcsec: f64, attempts: usize },
    Equipment(String) }`.
  - `pub const MAX_ATTEMPTS: usize = 50;`
  - `validate_params(&CenterOnTargetParams) -> Result<()>` —
    `tolerance > 0`, `max_attempts > 0`, `max_attempts <= MAX_ATTEMPTS`.
  - `pub fn haversine_arcsec(ra1_deg, dec1_deg, ra2_deg, dec2_deg) -> f64`
    — standalone for unit testability.
  - Driver traits: `PlateSolveOps`, `MountOps` (with `sync_to(ra,
    dec)` and `slew_to(ra, dec)` async methods). `CaptureOps` is
    pulled from `auto_focus.rs` — promote it to
    `imaging/tools/ops.rs` (new file) and re-export from both
    `auto_focus.rs` and `center_on_target.rs` to keep both call
    sites' import paths short.
  - `pub async fn run_center_on_target<C, P, M>(capturer, solver,
    mounter, params, emit_iteration: impl Fn(&IterationRecord))
    -> Result<CenterOnTargetResult, CenterOnTargetError>` — the
    pure driver. Loop body:
    1. `capture(duration)` → `document_id`.
    2. `solver.solve(document_id)` → `(solved_ra, solved_dec)`.
    3. If iteration index == 0: `mounter.sync_to(solved_ra,
       solved_dec)` and record `action = Sync`.
    4. Compute `residual_arcsec = haversine_arcsec(solved_ra,
       solved_dec, params.ra, params.dec)`.
    5. If `residual_arcsec <= params.tolerance_arcsec`: record
       `action = Converged` (overwrites `Sync` on iter 1 only when
       sync didn't happen, but per the previous bullet sync always
       fires on iter 1 — so the iter 1 record ends with whichever
       of `Sync→Converged` or `Sync→Slew` applies; the `action`
       field stores the *terminal* action for that iter).
    6. Else: `mounter.slew_to(params.ra, params.dec)`, record
       `action = Slew`, continue to next iter.
    7. Emit `centering_iteration` after each record via
       `emit_iteration(&record)`.
  - Synthetic-adapter unit tests: convergence on iter 1 (no slew),
    convergence on iter 3 (residual decreases each iteration), abort
    on mid-loop solve error, abort on mid-loop slew error,
    `tolerance_not_reached` after `max_attempts`, sync-fires-once
    invariant, `validate_params` rejection arms, haversine sanity
    (known coordinates → known arcseconds).
- [x] `services/rp/src/imaging/tools/ops.rs` (new — see decision
  above): hosts the shared `CaptureOps` trait. `auto_focus.rs`
  removes its local definition and re-imports.
- [x] `services/rp/src/imaging/tools/mod.rs`: `pub mod
  center_on_target; pub mod ops;`.
- [x] `services/rp/src/mcp/internals.rs`:
  - New `pub(crate) async fn do_sync_mount(&self, ra: f64, dec: f64)
    -> Result<(), String>` — resolve mount via `resolve_mount`,
    call `sync_to_coordinates`, map errors. Mirrors the
    `do_slew_blocking` shape minus the polling loop. Frees the
    primitive `sync_mount` MCP tool to call this helper too (small
    refactor inside `mount.rs:124-159`).
- [x] `services/rp/src/mcp/built_in/center_on_target.rs` (new):
  - `CenterOnTargetToolParams` (`#[serde(default)]` on every field
    so the body validator can produce input-order error messages).
  - `#[tool]` handler runs validation in order (camera_id → ra →
    dec → duration → tolerance_arcsec → max_attempts), resolves
    the camera and mount, emits `centering_started` (carrying
    `camera_id`, `ra`, `dec`, `tolerance_arcsec`, `max_attempts`),
    constructs the `CenterOnTargetAdapter`, calls
    `imaging::tools::center_on_target::run_center_on_target`,
    emits `centering_complete` on success (carrying
    `final_error_arcsec`, `attempts`, `final_ra`, `final_dec`),
    returns the result.
  - `CenterOnTargetAdapter<'a>` implementing `CaptureOps` (via
    `do_capture`), `PlateSolveOps` (via in-process
    `McpHandler::plate_solve` with `use_mount_hints: true`), and
    `MountOps` (via `do_sync_mount` and `do_slew_blocking` with
    `settle_after = mount.config.settle_after_slew.unwrap_or_default()`).
    The `emit_iteration` closure passed into `run_center_on_target`
    calls `event_bus.emit("centering_iteration", …)`.
  - ~10 unit tests against synthetic adapters (paralleling
    `auto_focus`'s unit-test layout): validate_params arms (3),
    haversine known-value (2), run_center_on_target end-to-end
    (5: iter-1 convergence, iter-3 convergence, capture error mid-
    loop, slew error mid-loop, `tolerance_not_reached`).
- [x] `services/rp/src/mcp/handler.rs`: append
  `+ Self::tool_router_center_on_target()` to the `tool_router`
  sum (line 79–87).
- [x] `services/rp/src/mcp/built_in/mod.rs`: declare the new module.

**Step 4 — Activate BDD + cleanup.**

- [x] Remove `@wip` tag from `center_on_target.feature` line 1.
- [x] Run full suite (`cargo rail run --profile commit -q` per
  CLAUDE.md rule 4); confirm BDD count grows by ~14 scenarios on
  top of the current 235.
- [x] `cargo fmt`. `cargo build --all --all-targets --all-features
  --locked` for linker-visible code per CLAUDE.md rule 4. No new
  workspace deps expected (haversine is closed-form arithmetic on
  `f64`); confirm before committing — if any landed, run
  `CARGO_BAZEL_REPIN=1 bazel mod tidy`.
- [x] Plan-doc bookkeeping: flip Phase 6c-3 status to **complete**
  with the work-breakdown checkboxes ticked, exit-criteria block
  populated with concrete BDD/unit-test counts. Flip the
  `center_on_target` row in `rp.md` Built-in Tools — Compound table
  from `*Planned.*` to `Implemented.`.

###### Out of scope / deferred

- **`min_improvement_arcsec` early-exit.** Useful future addition
  (early exit when an iteration stops improving the residual), but
  not v1. Add as an optional parameter when the first concrete
  need surfaces.
- **Adaptive per-iteration exposure scaling.** v1 takes a single
  `duration`; if low star count blocks a solve, the caller re-runs
  with a longer duration.
- **Auto-recovery on mid-loop failures.** Workflows that want
  retry-with-longer-exposure or retry-with-relaxed-tolerance wrap
  the call themselves.
- **Dome / rotator coordination.** Out of scope for `rp`'s built-in
  tooling per `rp.md` Future Considerations.

###### Exit criteria

- ~14 new BDD scenarios green (~249/249 total in rp's BDD suite,
  was 235/235 post-Phase-6c-2).
- ~13 new lib tests across `services/rp/src/imaging/tools/center_on_target`
  (~10 unit tests for the pure driver) + ~3 unit tests in
  `services/rp/src/mcp/built_in/center_on_target` for the MCP
  validation arms.
- `cargo rail run --profile commit -q` clean workspace-wide,
  `cargo fmt` clean. No new crates.io deps expected.
- `iterations[].document_id` round-trips through
  `GET /api/documents/{id}` carrying a `wcs` section written by the
  per-iteration `plate_solve`.

##### Phase 6c sequencing notes

- 6c-prep, 6c-1, 6c-2, 6c-3 landed as independent PRs in that
  order between 2026-05-03 and 2026-05-09.
- 6c-1 unblocked 6c-2's HTTP contract; 6c-2 unblocked 6c-3's
  `PlateSolveOps` adapter. 6c-prep was independent of the plate-
  solver chain and landed in parallel.
- The catalog-merge / shadow-rule code change called out under
  Phase 6 ("Catalog-merge code change (small, deferred until first
  caller)") remains deferred; `center_on_target` is a built-in and
  does not require it.

#### Catalog-merge code change (small, deferred until first caller)

The shadow rule is documented but the catalog-merge logic in `rp`
that actually implements precedence (route by name to plugin when
shadowed, log at startup) has no caller today — `rp` doesn't yet
have any plugin tool-provider integration in code. When the first
shadowing plugin is integrated (or when plugin tool aggregation
lands in general), include the precedence + log line as part of
that work.

### Phase 7 — Unified document/image cache + UUID-suffixed filenames

Status: **complete.** Independent of Phase 6 — landed in parallel.

Today the document store and the image cache are separate structures
with different lifetime rules: pixels evict via LRU + MiB budget,
documents accumulate forever (no eviction). Lookups by `document_id`
go through the in-memory `DocumentStore` map, which is lost on `rp`
restart — sidecars on disk are a one-way archive that `rp` itself
cannot read back. This phase consolidates the two structures, ties
filenames to document ids, and turns the on-disk pair into the
durable source of truth.

#### Decisions resolved during design discussion

These are now in `docs/services/rp.md` (Persistence, Image and
Document Cache, Document Resolution) and should not be re-litigated:

- **8-char UUID suffix on filenames.** `<file_naming_pattern>_<uuid8>.fits`
  with matching `.json` sidecar. The suffix is appended by `rp` after
  applying the operator-controlled `file_naming_pattern`; operators do
  not include UUID tokens in the template. `<uuid8>` is the first 8
  hex characters of the document's full UUID v4. The suffix is the
  on-disk reverse-lookup key only — the API uses the full UUID
  throughout.
- **Full UUID embedded in three places:** API `document_id`, FITS
  primary HDU header `DOC_ID`, sidecar `id` field. Any of the three
  is authoritative; the FITS header is preferred when disambiguating
  ghost matches.
- **Truncation rationale.** Once disambiguation via `DOC_ID` exists,
  the relevant collision metric is *expected ghost matches per query*
  (`k/N`), not *birthday probability over the archive* (`k²/(2N)`).
  At `N = 2³²` and `k = 100,000`, ghost matches per query ≈ 2·10⁻⁵ —
  the disambiguation path runs for correctness but essentially never
  fires. 8 chars keeps filenames short while leaving ample headroom.
- **Unified cache.** Pixels and document share one cache entry
  (`CachedImage` gains a `document: RwLock<ExposureDocument>` field).
  One LRU + MiB budget covers the combined memory footprint. Eviction
  takes both. The standalone `DocumentStore` map is folded into the
  cache.
- **Lazy filesystem fallback on miss.** `readdir <data_directory>` ⇒
  filter for filenames matching `_<uuid8>.fits` ⇒ verify by reading
  the FITS header `DOC_ID` against the requested full UUID ⇒ if FITS
  unreadable, fall back to the sidecar's `id` field ⇒ on match, read
  both files and re-populate the cache. No on-disk index file, no
  startup scan.
- **Live-as-long-as-on-disk contract.** After eviction or `rp`
  restart, a document remains addressable by id as long as its
  FITS+sidecar pair sits in `<data_directory>`. The contract operators
  see is "the file is the artifact"; in-memory caching is invisible
  performance behavior.

#### Work breakdown (in order)

- [x] **Step 1 — UUID suffix in capture filename.** `mcp.rs:capture`
      writes `<doc_uuid_8>.fits` (and matching `.json`). The optional
      `session.file_naming_pattern` config is reserved for a future
      operator-controlled template — until a token resolver lands the
      template is parsed but not rendered, so capture writes
      `<doc_uuid_8>.<ext>` regardless. New BDD-friendly unit test on
      the path shape; no existing scenarios pinned the literal
      `capture_*.fits` shape.
- [x] **Step 2 — `DOC_ID` FITS header.** `imaging::write_fits` accepts
      a `doc_id: &str` parameter and writes `DOC_ID` via
      `fitrs::Hdu::insert`. New `read_fits_doc_id(path)` returns
      `Ok(Some)` for new files, `Ok(None)` for legacy files without
      the keyword, `Err` for I/O / parse failures. Round-trip and
      legacy-file unit tests pin the contract.
- [x] **Step 3 — Embed the document in `CachedImage`.** Document lives
      inline behind `tokio::sync::RwLock`. `CachedImage::nbytes()`
      includes `serde_json::to_vec(&doc).len()` via an
      `AtomicUsize::json_nbytes` field that the cache mutex can read
      during eviction without taking the per-entry lock.
- [x] **Step 4 — Mediate document operations through the cache.**
      `DocumentStore` deleted. `ImageCache::put_section` holds the
      per-entry write lock across the sidecar write so concurrent
      updates serialize at the entry level, with rollback on write
      failure. `AppState.documents` and `McpHandler.documents` fields
      removed; capture, the five image-analysis tools, and the three
      document/image route handlers all route through the cache.
- [x] **Step 5 — Filesystem fallback on miss.** `ImageCache::resolve`
      and `resolve_document` scan `<data_directory>` for files whose
      filename suffix matches the document's UUID-8, verify each
      candidate via FITS `DOC_ID` (sidecar `id` as fallback authority
      for files without DOC_ID), rehydrate both pixels and document
      into the cache as MRU, and return. `resolve` declines when the
      sidecar's `max_adu` is null; `resolve_document` returns the doc
      anyway so callers can reach `file_path` for direct FITS reads.
      Six unit tests cover post-eviction rehydration, ghost-match
      disambiguation, max_adu-null handling, and sidecar-id fallback.
- [x] **Step 6 — BDD: `document_http_api.feature`.** Five scenarios:
      body shape after capture, 404 for unknown id, section
      round-trip via `measure_basic`, post-eviction on-disk fallback
      (`cache_max_images: 1`), cross-restart on-disk fallback (pin
      data_directory, capture, restart rp, fetch original).
      `RpConfigBuilder` extended with `with_data_directory` and
      `with_imaging`.

`rmpfit` and `ndarray-ndimage` are unaffected.

**Exit criteria met:** 139/139 BDD scenarios green (was 130/130 pre-
Phase-7 + 4 image_http_api); 154 lib tests green (added 16 across
Phase 7's six steps — capture path-shape, DOC_ID round-trip / legacy /
nonexistent, json-bytes accounting, six disk-resolve cases including
ghost-match and sidecar-id fallback); `cargo rail run --profile
commit -q` reports 530/530 passing workspace-wide. No new workspace
deps; no `bazel mod tidy` needed.

#### Out of scope for this phase

- Renaming pre-existing files. Greenfield design — no files exist
  pre-feature, no migration code path.
- Pinning a `data_directory` history (entries from old directories
  remain on disk but unreachable by id after the directory changes).
  This is the documented contract; revisit only if a real workflow
  needs it.
- Boot-time directory scan to pre-populate the cache. The lazy
  fallback on miss is sufficient and avoids paying scan cost up
  front. A pre-warm flag could be added later if profiling shows
  first-access latency matters.

## Out of scope / deferred

- **Andor / Hamamatsu / sCMOS HDR support.** The `CachedPixels::I32`
  variant is in place, but the connect-time selection logic and any
  driver-specific wiring are deferred until we have a real device to
  test against.
- **Cache LRU forced-eviction BDD scenario.** A unit test on `cache.rs`
  is the right place for LRU correctness; the end-to-end fallback path
  is exercised implicitly by Phase 4's scenarios.
- **Catalog-merge / shadow-rule code change.** Documented in `rp.md`
  but no caller yet (no plugin tool-provider integration exists).
  Lands with the first plugin that shadows a built-in tool.
- **SEP / sep-sys.** Considered and rejected during design (LGPL +
  C FFI burden) — see Image Analysis Strategy / Design Rationale.
- **Multi-camera image cache** with per-camera quotas. Single global
  LRU is sufficient for now.

(The **Plate-solver rp-managed service** entry that previously sat
here has been removed: ADR-005 resolved the ASTAP-vs-astrometry.net
question, the service shipped via [`plate-solver.md`](plate-solver.md),
and `plate_solve` + `center_on_target` shipped on top of it in
Phases 6c-2 / 6c-3.)

## Dependencies on other workstreams

- **Bazel:** every workspace `Cargo.toml` change needs
  `CARGO_BAZEL_REPIN=1 bazel mod tidy` (CLAUDE.md rule 10). Bazel
  remains shadow during the migration; not a blocker.
- **calibrator-flats:** no impact — it uses `compute_image_stats`,
  not `measure_basic`. Adding new tools doesn't break the existing
  contract.

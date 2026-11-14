# ADR-008: ZWO `zwo-camera` driver — author-maintained FFI crates + MIT-SDK public caching

## Status

Accepted (2026-06-14); implemented through Phase E (2026-06-16). Tracked by
[`docs/plans/zwo-driver.md`](../plans/zwo-driver.md) and specified in
[`docs/services/zwo-camera.md`](../services/zwo-camera.md). The FFI crates
([`zwo-rs`](https://github.com/ivonnyssen/zwo-rs) + `libzwo-sys`) are stood up as
a standalone repo (bindgen skeleton, green CI), and `services/zwo-camera` now
implements the full ASCOM `Device + Camera` surface over them (Phase C scaffold →
Phase E full Camera, ConformU passing), with the native-SDK CI/Bazel provisioning
this ADR calls for in place. **Phase G (ConformU CI wiring) landed 2026-06-18** —
`conformu.yml` provisions the ZWO SDK and runs zwo-camera's ConformU on
ubuntu/macOS/Windows like every other service. Remaining: EFW `FilterWheel`
(Phase F).

**Amended by [ADR-010](010-vendor-zwo-rs.md) (2026-06-17):** the "standalone repo,
consumed as an external git-rev dependency" canonical-home decision here is
reversed — `zwo-rs` + `libzwo-sys` are vendored first-party (nested, dual-homed)
at `crates/zwo-rs/`, with an explicit Bazel real/sim two-variant build. The
native-SDK provisioning, MIT public-cache, and link-model decisions in this ADR
are unchanged.

## Context

rusty-photon needs a first-class ASCOM Alpaca driver for ZWO ASI cameras and EFW
filter wheels — the ZWO analogue of the in-design
[`qhy-camera`](../services/qhy-camera.md). The workspace is **100% pure-Rust at
the link layer** since the `cfitsio` purge
([ADR-001 Amendment A](001-fits-file-support.md)). `qhy-camera` is the **first**
native-SDK exception to that posture; `zwo-camera` is the **second**. Two facts
that held for QHY are **inverted** for ZWO, and one is the same — and those
inversions drive this decision:

| Concern | QHY (the precedent) | ZWO |
|---|---|---|
| **SDK license** | Closed/proprietary; redistribution terms *unresolved* | **MIT** ("Copyright 2015, ZWO Company"), confirmed via the INDI/Debian redistribution |
| **Rust FFI layer** | Mature published `qhyccd-rs` / `libqhyccd-sys` already exist | **No usable equivalent** exists |
| **Build/link gating** | Native lib links at compile time on *every* machine | **Same** — SDK required at link time even with `--features simulation` |

On the FFI gap (the single biggest finding): the only Rust code covering camera +
EFW is `devDucks/asi-rs`, which is **GPL-3.0** (incompatible with our MIT/Apache),
unpublished, synchronous, and MQTT-daemon-shaped. The camera-only options
(`generic-camera-asi`, `cameraunit_asi`, `smroid/asi_camera2`,
`GreatAttractor/libasicamera-sys`) are variously stale, single-arch, unpublished,
or do not vendor the SDK. **No Rust EAF binding exists at all.** Unlike
`qhyccd-rs`, the ASI SDK also ships **no simulation backend**.

So rusty-photon cannot follow the `qhy-camera` shape of "consume a published FFI
crate and just write the device layer." It must first stand up the FFI itself.

## Decision

### 1. Author and maintain two new external FFI crates

Build **`zwo-rs`** (safe wrapper) + **`libzwo-sys`** (raw `bindgen` over
`ASICamera2.h` + `EFW_filter.h`; `EAF_focuser.h` later), as external siblings to
`qhyccd-rs` / `libqhyccd-sys`. Each is its **own repo**, published to crates.io —
**not** vendored into the monorepo tree and **not** a git submodule (consistent
with how `external/` crates and the `ascom-alpaca` git dep are consumed). Both
names are confirmed available on crates.io.

- **`simulation` feature lives in `zwo-rs`**, covering **camera + EFW**, fabricating
  frames + EFW position/moving — because the ASI SDK has no simulation mode (it
  mirrors what `qhyccd-rs`'s sim provides for free).
- **Lockstep development:** the workspace pins `zwo-rs` at a git rev
  (`[workspace.dependencies]`, `{ workspace = true }` per service), swapped to a
  crates.io `= "=0.1.x"` before the consuming PR merges. Local edit loop via an
  uncommitted `.cargo/config.toml` `paths = ["../zwo-rs"]`.

### 2. Unconditional native link (`simulation` is camera-free, not SDK-free)

> **Superseded by [ADR-014](014-zwo-per-device-services-and-link-features.md)
> (2026-07-10):** the link directives are now gated per device by additive
> Cargo features (`camera` / `efw` / `focuser`, default = all), so each
> service links only its own SDK. The paragraph below records the original
> decision; its second half — `simulation` removes the *camera*, never the
> *link* — still holds unchanged.

`libzwo-sys`'s `build.rs` emits the link for `ASICamera2` + `EFWFilter` +
`dylib=usb-1.0` (plus `stdc++`/`c++`, `udev`/IOKit) **with no feature/cfg gate**,
mirroring `libqhyccd-sys`. The `simulation` feature removes the *camera*
requirement at runtime, not the *SDK* requirement at link time. Therefore
**`cargo build -p zwo-camera` requires the SDK present on every compiling
machine** — dev laptops, CI runners, Bazel actions — exactly like `qhy-camera`.

### 3. MIT license → SDK blob on the **public** cache tier

ZWO's SDK is MIT, so — unlike the QHY blob, which must live behind the
authenticated/internal cache tier pending its redistribution-terms question — the
ZWO SDK may be cached/redistributed on the **anonymous-read public** mirror
(`cache.rustyphoton.space`, see
[`bazel-remote-cache.md`](../skills/bazel-remote-cache.md)). The MIT attribution
notice must travel with any cached blob (ZWO's own archive ships only a README;
the `license.txt` attribution comes from packagers). The per-arch matrix covers
all targets: `x64`, `armv8` (Pi 5), `mac_arm64` (Apple Silicon), and others.

### 4. `zwo-camera` is native-link exception #2; Bazel `requires-cargo` first

The service is tagged `requires-cargo` in Bazel (kept out of `bazel test //...`
by `.bazelrc`'s default `-requires-cargo`) until a hand-written `crate.annotation`
for `libzwo-sys` Bazel-izes the native link (link-search → SDK, `ASICamera2` +
`EFWFilter` + `dylib=usb-1.0`). This is the first prebuilt-proprietary-blob link
under Bazel (today's only sys crate, `aws-lc-sys`, *compiles* vendored C rather
than linking a blob); whichever vendor is Bazel-ized first pays that cost once. A
`zwo-rs` rev/version bump is an ordinary `Cargo.toml`/`Cargo.lock` delta, so it
rebuilds under `cargo rail`'s `commit` profile with no special handling; run
`CARGO_BAZEL_REPIN=1 bazel mod tidy && bazel mod tidy` (Rule 10) on each change.

## Consequences

- **More up-front work, less legal friction.** We build the FFI QHY got for free
  (the long pole — 3-arch linking, a hand-written simulator, the first
  prebuilt-blob Bazel annotation), but ZWO is legally much easier: MIT,
  redistributable, all target arches shipped, public-cache-eligible.
- **The device-trait layer is *easier* than QHY.** A cleaner C API and more ASCOM
  features map natively — `StopExposure` works (graceful, data-preserving →
  `CanStopExposure = true` *and* `CanAbortExposure = true`), native `PulseGuide`
  via ST4 (`CanPulseGuide = true`), native `ElectronsPerADU`, and trivial
  `PixelSizeX == PixelSizeY`. See
  [`zwo-camera.md`](../services/zwo-camera.md#ascom-camera-surface).
- **A new native build-dep.** Devs without the SDK build the rest of the
  workspace normally; `cargo rail`'s affected-packages-only mode builds
  `zwo-camera` only when its files change. CI / the Pi 5 nightly runner gain an
  SDK-provisioning step (pull from the public cache + `libusb-1.0-0-dev` + udev
  `99-asi.rules`); written generically so the separate `qhy-camera` work reuses
  it.
- **Two author-maintained, pre-1.0 FFI crates to track** — pinned exactly and
  swapped to published versions before merge.
- **Reusability.** Standing up `zwo-rs`/`libzwo-sys` also unlocks the deferred ZWO
  **EAF focuser** and **video mode** paths, and a future **CAA rotator**, without
  re-solving the FFI/link problem.

## Relationship to other decisions

- **[ADR-001 Amendment A](001-fits-file-support.md)** — establishes the pure-Rust
  / no-native-dep link posture. `zwo-camera` is the **second** sanctioned
  exception (after `qhy-camera`), justified by first-class ZWO hardware support.
- **[`qhy-camera`](../services/qhy-camera.md)** — the same native-SDK gating shape;
  its own ADR remains deferred. This ADR is intentionally **ZWO-scoped** and does
  not retroactively govern `qhy-camera`.

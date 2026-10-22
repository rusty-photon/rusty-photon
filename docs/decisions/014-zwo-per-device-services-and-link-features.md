# ADR-014: One service per ZWO device; per-device SDK link features

## Status

Accepted (2026-07-10). Amends [ADR-008](008-zwo-camera-native-sdk-ffi.md)
§"Unconditional native link" and re-scopes Phase F of
[`docs/plans/zwo-driver.md`](../plans/zwo-driver.md).

## Context

Installing `rusty-photon-zwo-camera` and `rusty-photon-zwo-focuser` together
failed: both debs shipped **all three** ZWO SDK blobs (`libASICamera2.so`,
`libEFWFilter.so`, `libEAFFocuser.so`) at the identical path
`/usr/lib/rusty-photon/`, and dpkg refuses two owners of one file. The root
cause sat below packaging: `libzwo-sys`'s `build.rs` emitted all three link
directives unconditionally, so *every* consumer binary linked — and therefore
had to ship — every SDK, whichever single device it actually drove.

That unconditional link was a deliberate simplicity carryover
(ADR-008), not a technical necessity: ZWO's three device SDKs are
**independent libraries with no shared handle** (the zwo-driver plan
records this), and the vendored headers are self-contained. Contrast QHYCCD,
where the filter wheel is wired through the *camera's* SDK handle
(`Control::CfwPort`) — there the hardware genuinely forces camera + wheel
into one process and one link.

The same fault line runs through service topology. `zwo-focuser` was made a
separate service because an EAF is an independently usable device, routinely
paired with non-ZWO cameras. Everything in that argument applies verbatim to
the EFW filter wheel (own USB port, own SDK, mix-and-match reality) — yet the
plan had EFW folding into `zwo-camera` merely because linking was
all-or-nothing anyway.

## Decision

1. **Topology principle: one service per independently usable device.**
   Device types are bundled into one service only when the hardware forces it
   (QHY camera + CFW share one SDK handle — stays bundled). ZWO's devices are
   independent, so: `zwo-camera` (ASI, port 11122), `zwo-focuser` (EAF, port
   11124), and — when built — a **separate `zwo-filterwheel` service** for the
   EFW (Phase F of the zwo-driver plan is re-scoped accordingly; the
   `filterwheel.enabled` config toggle and per-device `filter_names` override
   are removed from `zwo-camera`).
2. **Per-device link features in `libzwo-sys`/`zwo-rs`:** additive Cargo
   features `camera` / `efw` / `focuser` gate the per-SDK link directives in
   `build.rs` (via `CARGO_FEATURE_*`) and the matching `zwo-rs` module +
   `Sdk` surface. **Default = all three** (the pre-split behaviour for
   bare/external builds); the workspace dep sets `default-features = false`
   and each service enables exactly its own device. `ZWO_SKIP_NATIVE_LINK`
   stays env-gated as the master skip — the features are safe as features
   *because they are additive*: `--all-features` yields the full link, never
   a silently SDK-free build. Support libraries follow the blobs'
   **undefined-symbol tables**, not just DT_NEEDED (the EFW/EAF blobs
   reference `udev_*` symbols *without* declaring libudev — verified via
   `nm -D --undefined-only` on the x86_64 + aarch64 blobs): `camera` →
   `libusb-1.0`; `efw`/`focuser` → `libudev`; all three → the C++ runtime.
3. **Each package ships exactly its own blob** at the shared
   `/usr/lib/rusty-photon/` path (ADR-013's RUNPATH mechanism unchanged):
   zwo-camera → `libASICamera2.so`, zwo-focuser → `libEAFFocuser.so`. No
   file overlap → the debs co-install. `cargo ci build-packages` builds
   each zwo service in its **own cargo invocation** (cargo unifies features
   per invocation; batching the two would re-union the links) and
   `cargo ci verify-packages` proves via `ldd` that each binary resolves
   its own blob and does **not** link the others'.
4. **Bazel builds the union** (`crate_features = ["camera", "efw",
   "focuser"]` on the shared `libzwo-sys`/`zwo-rs` targets): one target set
   instead of a per-service variant explosion, and every SDK's link path
   stays continuously verified in CI, which stages all the blobs anyway
   (`install-zwo-sdk`). Per-service narrowing is enforced on the cargo
   path, which is what packaging ships.

## Consequences

- The two zwo debs install side by side; a future `zwo-filterwheel` package
  slots in with `libEFWFilter.so` and no new mechanism.
- Each package's `Depends:` shrinks to its blob's real needs
  (zwo-focuser drops `libusb-1.0-0`; zwo-camera drops `libudev1`).
- `cargo build -p zwo-focuser` on a dev machine needs only `libEAFFocuser`
  staged, not the full SDK set. Workspace-level builds (`cargo test
  --workspace`, `--all-features`, Bazel) still link the union — CI
  provisioning is unchanged.
- The `efw` feature has no in-repo consumer until the `zwo-filterwheel`
  service exists; its link recipe is exercised by Bazel and
  `--all-features` builds in the meantime.
- The published `zwo-rs`/`libzwo-sys` crates gain the feature split with
  defaults preserving the old surface — a semver-compatible addition for
  default-features users.
- Config-schema break in `zwo-camera` (the `filterwheel` section and
  `filter_names` override are gone) — acceptable pre-1.0, per the project's
  no-back-compat stance.

## References

- Plan: [`docs/plans/zwo-driver.md`](../plans/zwo-driver.md) (Phase F
  re-scoped; "separate libraries with no shared handle")
- Packaging mechanism: [ADR-013](013-native-sdk-payload-policy.md) (RUNPATH,
  private libdir, explicit `depends`)
- Link ground truth: `crates/zwo-rs/libzwo-sys/build.rs`; blob DT_NEEDED +
  undefined-symbol tables verified 2026-07-10 on the x64 and armv8
  indi-3rdparty blobs
- QHY contrast (hardware-forced bundling):
  `docs/services/qhy-camera.md` "shared camera handle" /
  `services/qhy-camera/src/backend.rs` (`Control::CfwPort`)

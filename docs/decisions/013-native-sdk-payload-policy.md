# ADR-013: Native SDK payload policy — redistribute ZWO, download QHY

## Status

Accepted (2026-07-04); implementation planned in phases PR-4 / PR-5 of
[`docs/plans/archive/service-packaging.md`](../plans/archive/service-packaging.md).
Resolves the redistribution question left open by
[ADR-009](009-vendor-qhyccd-rs.md).

## Context

Two camera services link vendor SDKs ([ADR-008](008-zwo-camera-native-sdk-ffi.md),
[ADR-009](009-vendor-qhyccd-rs.md), [ADR-010](010-vendor-zwo-rs.md)), and
their `.deb`/`.rpm` packages must get the runtime pieces onto target
machines. The two SDKs differ in exactly the two dimensions that matter:

|  | QHYCCD | ZWO ASI/EFW |
|--|--------|-------------|
| License | Proprietary, redistribution rights unresolved | MIT |
| Linkage | `libqhyccd.a` **static** → baked into our binary | `libASICamera2.so` / `libEFWFilter.so` **dynamic** |
| Runtime needs beyond libusb/libstdc++ | Camera **firmware files** + udev device access | The two `.so` blobs (no SONAME) + libudev + udev device access |

Publishing proprietary QHYCCD firmware inside our public GitHub artifacts
would be redistribution we have no right to. Meanwhile ZWO's MIT blobs are
explicitly redistributable (the same blobs ship in indi-3rdparty).

## Decision

1. **ZWO: redistribute in-package.** `rusty-photon-zwo-camera` bundles
   `libASICamera2.so` + `libEFWFilter.so` (taken at build time from the same
   pinned indi-3rdparty commit CI uses) into the private libdir
   `/usr/lib/rusty-photon/`, plus the MIT license text in the package
   docdir. Because the blobs carry no SONAME, loader resolution uses
   **RUNPATH** (`-Wl,-rpath,/usr/lib/rusty-photon`, injected via RUSTFLAGS
   by the package build script — not `build.rs`, keeping Bazel untouched;
   not `ld.so.conf.d`, which is unreliable without SONAMEs; not
   `LD_LIBRARY_PATH` units). The deb declares explicit `depends` because
   dpkg-shlibdeps cannot resolve SONAME-less private libs.
2. **QHY: never redistribute; download on the target.**
   `rusty-photon-qhy-camera` ships our binary (SDK statically linked — the
   *library* needs no separate distribution) plus
   `/usr/sbin/rusty-photon-qhy-firmware-install`, a root-only helper that
   downloads the SDK archive **pinned to the same version CI pins**
   (26.06.04), verifies a **pinned sha256**, and installs only the firmware
   files to `/lib/firmware/qhy`. The package postinst prints a pointer but
   never downloads (offline installs must not fail). Model: Debian's
   downloader packages for non-redistributable payloads.
3. **udev rules are authored by us, in both packages** (trivial
   vendor-ID match rules; no copied SDK content): group-scoped access
   (`GROUP="rusty-photon", MODE="0660"` — the service account's own group,
   never world-writable 0666 and never Debian's `plugdev`, which
   rpm-family hosts lack) plus the usbfs memory bump, installed under
   `/usr/lib/udev/rules.d/`. The camera units need no
   `SupplementaryGroups=` — the service's primary group owns the nodes.

## Consequences

- No proprietary bytes in our published artifacts; the QHY pin+hash makes
  the downloaded payload reproducible and tamper-evident.
- QHY operators run one extra documented command
  (`rusty-photon-qhy-firmware-install`) before first camera use; ZWO
  operators need nothing extra.
- The QHY SDK version is pinned in two shipped places (build script,
  firmware helper) — `scripts/check-pkg-assets.sh` asserts they match, and
  an SDK bump is a deliberate two-line PR.
- zwo-camera's lintian emits `custom-library-search-path` (RUNPATH) —
  documented as expected, not suppressed.
- If QHYCCD ever grants explicit redistribution permission, the helper can
  be replaced by a firmware payload in the package with no other layout
  change.

## References

- Plan: [`docs/plans/archive/service-packaging.md`](../plans/archive/service-packaging.md)
- SDK linkage ground truth: `crates/qhyccd-rs/libqhyccd-sys/build.rs`
  (static + `QHYCCD_SDK_DIR`), `crates/zwo-rs/libzwo-sys/build.rs`
  (dynamic + `ZWO_SDK_LIB_DIR`)
- CI provisioning precedents: `ivonnyssen/qhyccd-sdk-install@v4` (archive
  naming/URLs), `.github/actions/install-zwo-sdk` (pinned indi-3rdparty
  blob source)
- Prior SDK ADRs: [ADR-008](008-zwo-camera-native-sdk-ffi.md),
  [ADR-009](009-vendor-qhyccd-rs.md), [ADR-010](010-vendor-zwo-rs.md)

# ADR-018: SVBony SDK payload policy — a third, no-license bucket

## Status

Accepted (2026-07-21); implemented in Phase G of
[`docs/plans/archive/svbony-camera.md`](../plans/archive/svbony-camera.md) (landed
2026-07-21). Extends
[ADR-013](013-native-sdk-payload-policy.md)'s two-bucket framework
(redistribute-MIT-ZWO / download-proprietary-QHY) with a third bucket for
SDKs that carry **no license grant at all**.

## Context

[ADR-013](013-native-sdk-payload-policy.md) established two buckets for
native camera SDK payloads, distinguished by license:

|  | QHYCCD | ZWO ASI/EFW |
|--|--------|-------------|
| License | Proprietary, redistribution rights unresolved | MIT |
| Bucket | Never redistribute; download on target | Redistribute in-package |

`svbony-camera` (this workspace's third native-SDK camera service, see
[`docs/plans/archive/svbony-camera.md`](../plans/archive/svbony-camera.md)) links the
SVBony camera SDK (`libSVBCameraSDK`, SDK version 1.13.4 as carried by
indi-3rdparty's `libsvbony`). That SDK does not fit either existing bucket
cleanly:

- It is not proprietary-with-unresolved-terms like QHY's — it is **entirely
  silent**. The header (`SVBCameraSDK.h`) carries no copyright notice, the
  indi-3rdparty packaging directory ships no `LICENSE`/`COPYING` file for
  the SDK itself (only openastroproject's GPLv3 covering their own
  packaging scripts), and SVBony's own website and SDK zip carry no visible
  license text either.
- It is not MIT like ZWO's, so ADR-013's "MIT permits public-cache
  redistribution" reasoning does not apply.
- Absent any written grant, the default legal position for software is
  **all rights reserved** — redistribution by indi-3rdparty is
  vendor-tolerated in practice (SVBony supplies SDK updates and has staff
  filing indi-3rdparty issues) but there is no license text anyone could
  point to as a redistribution *right*.

This "no license at all" case is legally *more* restrictive than QHY's
"proprietary, unresolved" case (QHY's terms are merely undocumented; SVBony's
are entirely absent), so it inherits QHY's posture rather than getting a
genuinely new one — but it is worth naming explicitly because the
mechanical delivery differs in one respect worth flagging (see
*Packaging simplification, to be confirmed*, below).

## Decision

1. **Never redistribute.** `rusty-photon-svbony-camera` does **not** bundle
   `libSVBCameraSDK.so` in its `.deb`/`.rpm` payload, mirroring
   ADR-013's QHY bucket exactly — absent a written grant, treat the default
   "all rights reserved" as binding.
2. **Deliver via a root-only download-on-target helper**, analogous to
   `rusty-photon-qhy-firmware-install`
   (`services/qhy-camera/pkg/rusty-photon-qhy-firmware-install`):
   [`rusty-photon-svbony-sdk-install`](../../services/svbony-camera/pkg/rusty-photon-svbony-sdk-install)
   (Phase G, landed 2026-07-21) downloads the SVBony SDK blob from the
   pinned indi-3rdparty commit `.github/actions/install-svbony-sdk` (CI)
   also uses, verifies a **pinned sha256** (computed directly against the
   real blobs, per architecture — amd64 and armv8), and installs
   `libSVBCameraSDK.so` to `/usr/lib/rusty-photon/` — the same model as
   QHY's firmware-install helper: package postinst prints a pointer but
   never downloads (offline installs must not fail).
   **Where this bites harder than QHY's** (issue #704, 2026-07-26): QHY
   links its SDK *statically* and defers only camera firmware, so a
   qhy-camera install always starts and merely finds no device. SVBony's
   SDK exists only as a `.so`, so the deferred piece is a link-time
   `NEEDED` entry: until the helper runs, the loader kills the binary
   with exit 127 and the unit's `Restart=on-failure`/`RestartSec=5`
   retries — the same shape as a serial driver retrying an absent device,
   and self-healing within seconds of the helper finishing, with no
   operator `systemctl` step. `scripts/verify-packages.sh` walks that
   whole bootstrap on both package flavors — blob absent from the payload,
   helper, service active, Alpaca probe, plus an `ldd` proof that the
   installed blob resolves through the RUNPATH. Removing the asymmetry
   entirely would mean `dlopen`ing the SDK instead of linking it, so the
   service starts and reports "SDK not installed" like any other missing
   device; that is a libsvbony-sys rework worth its own issue, deliberately
   out of scope here.
3. **udev rules are authored by us**, group-scoped
   (`GROUP="rusty-photon", MODE="0660"`, never `MODE="0666"`) per
   [ADR-013 §3](013-native-sdk-payload-policy.md), installed under
   `/usr/lib/udev/rules.d/` — `pkg/90-rusty-photon-svbony.rules` (VID
   `f266`), landed in Phase C alongside the bare service skeleton.
   **Unpackaged hosts get one from the helper** (issue #710): a developer
   box running the binary from a checkout has no package and therefore no
   rule, and libusb then cannot open the device node at all (`errno=13`,
   zero cameras enumerated). Where no packaged rule is present the helper
   writes `/etc/udev/rules.d/90-rusty-photon-svbony-dev.rules` —
   `TAG+="uaccess"` plus a resolvable `GROUP`/`MODE="0660"` pair, still
   never `MODE="0666"`, under a deliberately *different* basename so it
   can never mask the packaged rule.
4. **Packaging simplification, corrected by Phase F CI provisioning work;
   Phase G's runtime call: RUNPATH, not `ldconfig`.**
   indi-3rdparty's `libsvbony/CMakeLists.txt` sets a CMake **install**
   property (`SOVERSION 1`), which earlier drafts of this ADR and
   `docs/plans/archive/svbony-camera.md` read as "the blob carries a proper
   versioned SONAME" — Phase F's `install-svbony-sdk` CI action
   byte-verified the actual vendored `.bin` (`readelf -d`) and found **no
   embedded DT_SONAME at all**, same as ZWO's blobs. What Phase F confirmed
   empirically instead: glibc's `ldconfig` falls back to the on-disk
   *filename* as its cache key when a shared object has no SONAME, so
   installing under `libSVBCameraSDK.so.1` (+ an unversioned `.so` symlink)
   and running `ldconfig` still resolves `-lSVBCameraSDK` via ordinary
   `ldconfig` mechanics with no RUNPATH injection needed — for CI's
   build-time purposes, which installs into a standard, `ldconfig`-scanned
   prefix (`/usr/local/lib`). **That does not carry over to the packaged
   runtime install**: `rusty-photon-svbony-sdk-install` installs into
   `/usr/lib/rusty-photon/`, this ADR's private, package-owned directory
   (point 2 above), which is deliberately **not** on `ldconfig`'s default
   scan path regardless of the SONAME finding. So Phase G's call is
   RUNPATH, matching ZWO's mechanism exactly: the packaged
   `rusty-photon-svbony-camera` binary is linked with
   `-Wl,-rpath,/usr/lib/rusty-photon` (mirroring
   `scripts/build-packages.sh`'s existing ZWO handling), and the helper
   installs the bare `libSVBCameraSDK.so` linker name with no SOVERSION/
   symlink pair — see `docs/services/svbony-camera.md`'s Packaging section
   and `install-svbony-sdk/svbony-sdk.mjs`'s header comment for the full trace.
   **Update (issue #679, 2026-07-22): wired into `scripts/build-packages.sh`.**
   A `needs_svbony` SDK-staging leg now stages the pinned blob for the link
   only (mirroring QHY/ZWO's staging, but — per this ADR — never copying it
   into `pkg/lib/` or bundling it in the package); the RUNPATH-linked binary
   side is now exercised end-to-end (verified: `readelf -d` on the built
   binary shows `NEEDED libSVBCameraSDK.so` + `RUNPATH
   /usr/lib/rusty-photon`). This fixed `nightly-packages`' Linux legs, which
   were failing with `unable to find library -lSVBCameraSDK` (the script
   discovered `svbony-camera` via its `pkg/` directory but staged no SDK for
   it). The Bazel-side SDK-fetch rule remains separately deferred — see
   `docs/plans/archive/svbony-camera.md`'s Status section. On macOS,
   `scripts/build-tarballs.sh`/`scripts/generate-brew-formulas.sh` exclude
   `svbony-camera` outright instead (no confirmed `mac_arm64` blob).
5. **If SVBony ever grants written redistribution permission** (worth an
   email — they are responsive to indi-3rdparty issues), this collapses to
   ADR-013's ZWO bucket with no layout change beyond adding the blob as a
   package asset.
6. **Windows extends the same bucket, byte-verified (PR #658 review,
   2026-07-21).** SVBony's own Windows `SVBCameraSDK` distribution
   (svbony.com/downloads/software-driver, `windows-SVBCameraSDK-v1.13.4.zip`
   — same SDK version already pinned for Linux) carries **no license/EULA
   text anywhere in the package** either (checked: `ReadMe.txt` is a pure
   changelog, no `LICENSE`/`COPYING` file, the header has no copyright
   notice) — the identical "entirely silent" posture this ADR already
   applies to the Linux/macOS blob, so point 1 (never redistribute) extends
   to it unchanged. Unlike the Linux/macOS blobs, though, **this download
   cannot be automated in CI at all**: the download button is
   `data-fileRestricted="true"` behind a `recaptcha-v3.js`/
   `unified-captcha.js` gate, not a plain fetchable URL like
   indi-3rdparty's GitHub-raw mirror or `install-zwo-sdk`'s Windows CDN
   input. `libsvbony-sys/build.rs` now has real Windows link directives
   (`crates/svbony-rs/libsvbony-sys/build.rs`, verified: the header's
   function set, and the `.lib`/`.dll` export tables for x86 + x64, are
   byte-identical in name and calling convention to what this crate already
   binds — plain `cdecl`, no `libusb` dependency, WinUSB-backed
   internally), so a human who manually downloads the SDK once and sets
   `SVBONY_SDK_LIB_DIR` can build the real Windows link today — and §7
   below is how CI provisions that same payload without a scriptable
   vendor URL. `bazel/windows-latest` still only exercises
   `SVBONY_SKIP_NATIVE_LINK=1` (the Bazel-side SDK-fetch rule remains
   deferred — see `docs/plans/archive/svbony-camera.md`'s Status section).
7. **A private, token-gated CI mirror carries the Windows payload
   (added 2026-07-29, issue #720 Part 2).** The captcha gate blocks
   *fetching*; points 1–6 govern *redistribution* — publishing the SDK in
   artifacts third parties can obtain. A mirror that is (a)
   access-controlled — every request, reads included, requires a bearer
   token held only by this repo's CI and its operators; (b) read-only from
   the network — the serving Worker has no upload path, objects are put
   out-of-band from an operator's authenticated Cloudflare session; and
   (c) absent from every published artifact, distributes nothing to any
   third party, so it sits *outside* the "never redistribute" scope rather
   than weakening it. Risk context: SVBony visibly tolerates fully
   *public* redistribution of these same payloads (indi-3rdparty vendors
   the Linux/macOS blobs at GitHub-raw URLs this project already fetches
   in CI; NINA's `nina.external` repo and PHD2's GitHub tree vendor the
   Windows DLL), so a private, access-controlled copy is strictly
   lower-exposure than the public mirrors already relied on. Mechanics:
   Cloudflare R2 + Worker at `https://private.rustyphoton.space` (code +
   runbook: `tools/private-mirror-worker/`), holding the
   operator-downloaded vendor originals (`windows-SVBCameraSDK-v1.13.4.zip`
   and the driver installer). The mirror is trusted for *availability
   only*: `install-svbony-sdk`'s Windows step verifies a pinned sha256
   computed from the vendor original before use. If SVBony ever objects,
   deleting the bucket ends the arrangement with no published artifact
   ever having carried the bytes.

## Consequences

- No unlicensed bytes in our published artifacts, matching the QHY
  precedent's risk posture.
- Windows CI now links + loads the real SDK (`native.yml`'s build + FFI
  smoke, `conformu.yml`'s real-link leg) via §7's private mirror. Hosted
  runners have no camera (and no vendor driver), so runtime coverage is
  DLL load + zero-camera enumeration — real-hardware ConformU on Windows
  remains a manual/operator concern (see the validation records under
  `docs/validation/`). Fork PRs and dependabot runs receive no
  `PRIVATE_MIRROR_TOKEN` secret and degrade to `SVBONY_SKIP_NATIVE_LINK=1`.
- SVBony operators run one extra documented command
  (`rusty-photon-svbony-sdk-install`) before first camera use, exactly like
  QHY operators today; ZWO operators still need nothing extra. Unlike QHY's
  case the pre-bootstrap service cannot start at all, so it restart-loops on
  a loader error until the helper runs — visible in the journal, harmless,
  and self-healing (no `systemctl` step) the moment the blob lands.
- The helper is the single bootstrap step on *both* kinds of host: on a
  packaged target it installs only the SDK (the package's udev rule is
  already in force), on a developer box it installs the SDK and a udev
  rule, so "the SDK is installed" and "the SDK can see the camera" can
  never drift apart.
- The SDK ref is pinned in the helper script
  (`services/svbony-camera/pkg/rusty-photon-svbony-sdk-install`) AND in
  `scripts/build-packages.sh` (issue #679), both cross-checked by
  `scripts/check-pkg-assets.sh` against `REF` in
  `.github/actions/install-svbony-sdk/svbony-sdk.mjs`, mirroring QHY's version/sha256 pin parity-check pattern — a
  version bump is a deliberate,
  checked PR.
- `svbony-camera`'s Bazel build still bakes `SVBONY_SKIP_NATIVE_LINK=1`
  unconditionally — Phase F added a plain-Cargo
  `.github/actions/install-svbony-sdk` CI provisioning action (wired into
  `conformu.yml`/`native.yml`), but it is a GitHub-Actions composite Bazel's
  hermetic build graph does not consume, and Phase G deliberately did not
  add a Bazel-side SDK-fetch repository rule (out of scope; not testable
  without the real SDK) — this Bazel-side simplification (distinct from
  this ADR's packaging decision) remains deferred follow-up work, recorded
  in `docs/plans/archive/svbony-camera.md`'s Status section and
  `crates/svbony-rs/libsvbony-sys/BUILD.bazel`.
- ADR-013's two-bucket framework is now a three-bucket framework:
  redistribute (MIT-clear, e.g. ZWO), download-proprietary-unresolved (e.g.
  QHY), and download-no-license-at-all (SVBony, this ADR) — legally the
  latter two behave identically (never redistribute, download on target);
  they are named separately because the *reason* differs (unresolved terms
  vs. no terms at all) and a future SDK might need the distinction (e.g. if
  a vendor's silence is later found to carry an implied license under some
  jurisdiction's law, which "unresolved proprietary terms" would not).

## References

- [ADR-013](013-native-sdk-payload-policy.md) — the two-bucket framework
  this ADR extends
- [ADR-009](009-vendor-qhyccd-rs.md), [ADR-008](008-zwo-camera-native-sdk-ffi.md),
  [ADR-010](010-vendor-zwo-rs.md) — the prior native-SDK ADRs
- Plan: [`docs/plans/archive/svbony-camera.md`](../plans/archive/svbony-camera.md) — the
  full SVBony decision record, including the "Verified SDK facts" section
  this ADR's license findings are drawn from
- Design doc: [`docs/services/svbony-camera.md`](../services/svbony-camera.md)
- Downloader-helper shape reference:
  `services/qhy-camera/pkg/rusty-photon-qhy-firmware-install`
- SDK ground truth: indi-3rdparty `libsvbony` (`SVBCameraSDK.h`, per-arch
  blobs, `CMakeLists.txt` — SDK 1.13.4, `.so` only, no license text found
  anywhere in the packaging)

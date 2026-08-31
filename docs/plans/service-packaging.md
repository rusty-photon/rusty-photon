# Service Packaging Plan — `.deb` / `.rpm` for the whole family

## Goal

Provide installable `.deb` and `.rpm` packages for every rusty-photon service
so an observatory machine (first target: the Debian 13 arm64 test rig) can be
provisioned with `apt install` — no Rust toolchain, no hand-copied binaries —
with systemd supervising every service. This generalizes the proven
single-service pattern from
[`filemonitor-packaging.md`](archive/filemonitor-packaging.md) (PR #33),
superseding several of its per-service decisions; the architecture is recorded
in [ADR-012](../decisions/012-service-packaging-architecture.md) and the
native-SDK payload policy in
[ADR-013](../decisions/013-native-sdk-payload-policy.md).

Deployment is native packages, not containers, by explicit decision: the
drivers' USB/udev/firmware needs and ASCOM Alpaca's UDP discovery would force
`--network host` + privileged device passthrough, erasing container isolation
while keeping host-level setup anyway (see ADR-012 Context).

## Implementation Status

| Phase | Description | Status | Branch / PR |
|-------|-------------|--------|-------------|
| PR-1 | This plan + ADR-012 + ADR-013 + archive old plan | Merged | #435 |
| PR-2 | Migrate filemonitor to the new pattern (template proof) + filemonitor/sentinel XDG fix + `check-pkg-assets.sh` | Merged | #438 |
| PR-3 | 11 pure-Rust daemons + `phd2-guider` CLI package | Merged | #441 |
| PR-4 | `qhy-camera` package + firmware downloader helper | Merged | #444 |
| PR-5 | `zwo-camera` package with bundled MIT SDK blobs | Merged | #446 |
| PR-6 | `build-packages.sh` + `verify-packages.sh` + `docs/packaging.md`; first on-rig install | Merged | #448 |
| PR-7 | `release.yml` generalization (x86_64 matrix, Homebrew rename) | Deferred | |

Carried over from the superseded plan (still open, now owned by PR-7):
create the `ivonnyssen/homebrew-rusty-photon` tap content and add the
`HOMEBREW_TAP_TOKEN` secret. **Update:** the tap content, the per-service
formula generation (whole family + meta-formula, stable and nightly
channels), and the Homebrew rename all landed via
[nightly-releases.md](nightly-releases.md) N4 — what remains of PR-7 is
only the `release.yml` deb/rpm service-matrix generalization.

## Decisions (fixed — see ADR-012 / ADR-013 for rationale)

- **Scope:** all services. 14 systemd-supervised daemons + `phd2-guider`
  packaged as a plain CLI (it was a clap subcommand tool, not a daemon,
  when this plan shipped; issue #464 later gave it an HTTP service mode
  and a unit — see the update in § "phd2-guider (PR-3)" below, making it
  the 15th daemon). Test-double bins (`mock_phd2`, `mock_astap`) are
  never packaged.
- **Naming:** `rusty-photon-` prefix on packages **and** installed binaries
  **and** units: package `rusty-photon-<svc>`, `/usr/bin/rusty-photon-<svc>`,
  `rusty-photon-<svc>.service`. Cargo bin names are unchanged; the rename
  happens in the packaging asset mapping. filemonitor's bare-named
  package/binary/unit are renamed (breaking; acceptable pre-1.0).
- **Config is user-based XDG, owned by the service** — packages ship **no
  config file** and units pass **no `--config` flag**. Services resolve
  `~/.config/rusty-photon/<svc>.json` via `resolve_config_path`
  (`crates/rusty-photon-config`) and self-create it on first start
  (`materialize_identity`). With the shared system user's home at
  `/var/lib/rusty-photon`, live config lands at
  `/var/lib/rusty-photon/.config/rusty-photon/<svc>.json`. postinst creates
  an `/etc/rusty-photon` → `/var/lib/rusty-photon/.config/rusty-photon`
  symlink for operator discoverability. No dpkg conffiles anywhere (six
  services rewrite their config at runtime via `config.apply` /
  `materialize_identity`; conffile semantics cannot express that).
- **One shared system user** `rusty-photon` (home `/var/lib/rusty-photon`,
  no login shell). Hardware privileges are scoped per unit via
  `SupplementaryGroups=` for serial services (`dialout`, plus `plugdev` on
  deb only — see the per-class table), while camera packages assign device
  nodes to the account's own `rusty-photon` group via their udev rules.
  Enables the shared-home XDG config model and the rp ↔ plate-solver
  shared FITS tree, and keeps deb maintainer scripts byte-identical
  across packages.
- **Formats:** `.deb` + `.rpm` only for the family — on Linux. (This
  decision has since been superseded per-OS: the Windows suite MSI covers
  the family per ADR-015 / [windows-packaging.md](windows-packaging.md),
  and macOS ships family-wide Homebrew formulas per
  [nightly-releases.md](nightly-releases.md) N4.)
- **ARM64 packages are built natively on the rig for now** via
  `scripts/build-packages.sh`; CI packaging for arm64 is deferred (PR-7 keeps
  x86_64 in `release.yml`).
- **Native SDK payloads (ADR-013):** ZWO's MIT-licensed `.so` blobs are
  redistributed inside `rusty-photon-zwo-camera`; QHYCCD's proprietary
  firmware is **never** redistributed — a pinned, checksum-verified
  downloader helper installs it on the target machine.

## Design

### Asset layout — per-service `pkg/` dirs + a consistency checker

Plain committed files, no generator (explicitness over DRY; `git grep` must
not lie). Shared canonical copies live in `packaging/`; a checker keeps the
per-service instances convergent.

```
packaging/
  postinst.common        # canonical; every daemon pkg/postinst is byte-identical
  postinst.udev-stanza   # canonical camera addition (inserted before #DEBHELPER#)
  postrm.common          # canonical; byte-identical everywhere
  README.md              # invariants, how to add a service
scripts/
  check-pkg-assets.sh    # asserts the invariants below
  build-packages.sh      # on-device package build (PR-6)
  verify-packages.sh     # container install/upgrade/purge verification (PR-6)
services/<svc>/pkg/
  rusty-photon-<svc>.service
  postinst               # copy of packaging/postinst.common
  postrm                 # copy of packaging/postrm.common
```

Extras: `services/qhy-camera/pkg/` adds `90-rusty-photon-qhy.rules` +
`rusty-photon-qhy-firmware-install`; `services/zwo-camera/pkg/` adds
`90-rusty-photon-zwo.rules` (named 90- for family consistency; unlike QHY
there are no vendor rules to sort against), a committed
`ZWO-SDK-LICENSE.txt` (checker-verified copy of the license vendored with
the libzwo-sys headers), and a gitignored `lib/` dir into which the build
script stages the ZWO blobs before `cargo deb` runs.

`check-pkg-assets.sh` asserts, per daemon crate: unit file named
`rusty-photon-<dir>.service` with `ExecStart=/usr/bin/rusty-photon-<dir>`
(no `--config`); `postinst`/`postrm` byte-identical to the canonical copies
(or to the documented camera variant); `[package.metadata.deb] name`,
`unit-name`, and `[package.metadata.generate-rpm] name` all equal
`rusty-photon-<dir>`; reload-capable services have `ExecReload`; the QHY SDK
version pinned in `build-packages.sh` matches the one in
`rusty-photon-qhy-firmware-install`.

### systemd unit template

```ini
[Unit]
Description=Rusty Photon <svc> — <role> (port <port>)
After=network.target

[Service]
Type=simple
ExecStart=/usr/bin/rusty-photon-<svc>
ExecReload=/bin/kill -HUP $MAINPID        # reload-capable services only
Restart=on-failure
RestartSec=5
User=rusty-photon
Group=rusty-photon
Environment=RUST_LOG=info
Environment=HOME=/var/lib/rusty-photon
WorkingDirectory=/var/lib/rusty-photon
StateDirectory=rusty-photon/<svc>

# Hardening — hobby-rig level; must not break config write-back, serial, USB.
NoNewPrivileges=yes
ProtectSystem=strict
ReadWritePaths=/var/lib/rusty-photon
ProtectHome=yes
PrivateTmp=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictSUIDSGID=yes
LockPersonality=yes
RestrictRealtime=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
UMask=0027

[Install]
WantedBy=multi-user.target
```

Notes: `ConfigurationDirectory=` is deliberately **not** used (systemd
creates it root-owned, which would break the config crate's atomic
tmp+rename+fsync-dir write-back). `HOME` is set explicitly as belt-and-braces
even though the `dirs` crate falls back to the passwd entry.
`ReadWritePaths=/var/lib/rusty-photon` covers both the XDG config write-back
and the rp ↔ plate-solver shared FITS tree.

Per-class deltas:

| Class | Services | Delta |
|-------|----------|-------|
| serial | ppba-driver, qhy-focuser, pa-falcon-rotator, pa-scops-oag, dsd-fp2, star-adventurer-gti | per-flavor units: `SupplementaryGroups=dialout plugdev` (deb; plugdev is base-passwd's) vs `SupplementaryGroups=dialout` (rpm; plugdev is never created there) |
| USB camera | qhy-camera, zwo-camera | no supplementary groups (udev rule assigns nodes `GROUP="rusty-photon"`), `RestrictAddressFamilies` += `AF_NETLINK` (libusb hotplug), no `MemoryDenyWriteExecute` (vendor blob caution) |
| network-only | filemonitor, sentinel, rp, ui-htmx, plate-solver, calibrator-flats, sky-survey-camera | `PrivateDevices=yes`, `MemoryDenyWriteExecute=yes`; rp additionally gets `RestrictAddressFamilies` += `AF_NETLINK` (glibc `getifaddrs(3)` enumerates interfaces over netlink; rp needs it for the MCP `Host` allowlist) |

Reload-capable (get `ExecReload`): filemonitor, zwo-camera,
pa-falcon-rotator, star-adventurer-gti, dsd-fp2, qhy-camera, ppba-driver,
sky-survey-camera, qhy-focuser.

Ports (for unit descriptions): filemonitor 11111, ppba-driver 11112,
qhy-focuser 11113, sentinel 11114 (dashboard), rp 11115, sky-survey-camera
11116, star-adventurer-gti 11117 (its Alpaca port — 11880 is the mount's
own SynScan UDP wire port, not ours), pa-falcon-rotator 11118, dsd-fp2
11119, ui-htmx 11120, qhy-camera 11121, zwo-camera 11122, plate-solver
11131, calibrator-flats 11170.

**No-defaultable-config gate:** three services cannot self-create a
config — `sky-survey-camera` (optics fields are deliberately mandatory,
no `Config::default()`), `plate-solver` (`astap_binary_path` /
`astap_db_directory` point at operator-installed ASTAP), and
`calibrator-flats` (the config *is* a flat plan naming real device IDs).
Self-creating placeholder values would just move the failure somewhere
more confusing. Their units instead carry
`ConditionPathExists=/var/lib/rusty-photon/.config/rusty-photon/<svc>.json`:
on a fresh install the unit is skipped (condition failed — not an error,
no restart loop) until the operator writes the config; `systemctl start`
then works normally. `check-pkg-assets.sh` asserts the gate on exactly
these three, and asserts `ExecReload` on the reload-capable list above.

**Missing-SDK retry (not a gate):** `svbony-camera`'s binary links
`libSVBCameraSDK.so`, which
[ADR-018](../decisions/018-svbony-sdk-no-license-payload-policy.md)
forbids shipping — the operator installs it with
`rusty-photon-svbony-sdk-install` — so until then the binary cannot be
loaded at all and the unit restart-loops on exit 127. That is left as-is
deliberately: `Restart=on-failure` is the same retry the serial drivers
rely on for an absent device, and it heals the moment the blob lands, so
no `ConditionPathExists` gate (and no operator `systemctl start`) is
needed. What issue #704 fixed is the *verification* gap — nothing in
`verify-packages.sh` ever installed the SDK, so the service was held to
the ordinary "active + config + port" contract it could not meet. It now
walks the bootstrap for that one service: assert the payload withheld the
blob, run the packaged helper, then the ordinary contract (the
active-within-30s wait doubling as proof that the retry self-heals).

### Maintainer scripts

`packaging/postinst.common` (deb; byte-identical in every daemon package):

```sh
#!/bin/sh
set -e
if ! getent passwd rusty-photon > /dev/null; then
    adduser --system --group --home /var/lib/rusty-photon --quiet rusty-photon
fi
# Create the config directory chain too: /etc/rusty-photon points at it,
# and ConditionPathExists-gated services never start on a fresh install,
# so nothing else would create it before the operator writes a config.
install -d -m 0750 -o rusty-photon -g rusty-photon \
    /var/lib/rusty-photon \
    /var/lib/rusty-photon/.config \
    /var/lib/rusty-photon/.config/rusty-photon
if [ ! -e /etc/rusty-photon ]; then
    ln -s /var/lib/rusty-photon/.config/rusty-photon /etc/rusty-photon
fi
#DEBHELPER#
```

`packaging/postrm.common`:

```sh
#!/bin/sh
set -e
SVC="${DPKG_MAINTSCRIPT_PACKAGE#rusty-photon-}"
if [ "$1" = "purge" ]; then
    rm -f "/var/lib/rusty-photon/.config/rusty-photon/$SVC.json"
    rm -rf "/var/lib/rusty-photon/$SVC"
fi
#DEBHELPER#
```

The shared user, home, and `/etc/rusty-photon` symlink are never removed on
purge (shared across packages; Debian convention keeps system users). The
camera packages' postinst is the one sanctioned variant:
`postinst.common` with the canonical `packaging/postinst.udev-stanza`
inserted before `#DEBHELPER#` — `check-pkg-assets.sh` verifies the exact
byte-level construction.

RPM mirrors this via `post_install_script` / `pre_uninstall_script` /
`post_uninstall_script` (plain `systemctl` calls — cargo-generate-rpm does
not process rpm macros). rpm has **no purge lifecycle**: erase preserves the
runtime-created config and state (parity with `dpkg remove`); removing them
is a manual step to be documented in the operator guide (`docs/packaging.md`,
a PR-6 deliverable — it does not exist yet). Camera packages additionally
`getent group plugdev >/dev/null || groupadd -r plugdev` (the group is not
standard on RPM distros).

### Code changes required (PR-2 / PR-3)

Survey result: the 8 hardware drivers use `resolve_config_path` (+
`materialize_identity`), but **six services do not**: `filemonitor`
(CWD-relative `config.json` default), `sentinel` (loads config only with
`--config`, else silent in-memory defaults), and `rp` / `ui-htmx` /
`plate-solver` / `calibrator-flats`. All six adopt the same one-line bootstrap —
`rusty_photon_config::resolve_and_init("<svc>", args.config, &default)` —
the canonical composite that resolves the path and writes the typed default
config on first start (so a packaged install materializes an editable
file), logging `Created default config at …`. Self-creation applies **only
to the XDG default path**: an explicit `--config` naming a missing file
stays a hard error, baked into the helper so the fail-fast contract cannot
be miscopied (a typo'd path must never silently run on defaults;
filemonitor's integration test and BDD scenarios pin this). The underlying
primitives (`resolve_config_path`, `init_file_if_absent`) stay public.
filemonitor + sentinel landed in PR-2 (filemonitor also gained an
`impl Default for Config` based on its previously packaged default, watch
path moved to `/var/lib/rusty-photon/filemonitor/`).

PR-3 reality check on the remaining four: only `ui-htmx` and `rp` have a
meaningful default, so only they adopt `resolve_and_init`. `ui-htmx`
serializes `Config::default()`; `rp`'s config derives neither `Default`
nor `Serialize`, so its scaffold is a hand-written minimal JSON value
(`session.data_directory` under `/var/lib/rusty-photon/rp/`, empty
`equipment`, default `server`) — and a bare `rp` (no subcommand) now
serves instead of erroring, since the packaged unit runs a flag-less
binary. `plate-solver` and `calibrator-flats` have no defaultable config
(see the gate note above): they adopt `resolve_config_path` only — XDG
path resolution with `--config` now optional, missing file stays a clear
startup error — and their units are `ConditionPathExists`-gated, as is
`sky-survey-camera`'s (already on `resolve_config_path`; no code change).

### qhy-camera (PR-4)

- `libqhyccd.a` is linked **statically** → runtime NEEDED is only
  `libusb-1.0.so.0` + `libstdc++.so.6`; deb `depends = "$auto"` resolves them
  via dpkg-shlibdeps.
- Own-authored udev rule `90-rusty-photon-qhy.rules` →
  `/usr/lib/udev/rules.d/` (never `/etc/udev/rules.d` from a package). It
  deliberately sorts **after** the SDK's `85-qhyccd.rules` so its
  `GROUP="plugdev", MODE="0660"` tightens the SDK rules' trailing blanket
  `MODE="0666"`:

  ```
  SUBSYSTEMS=="usb", ATTRS{idVendor}=="1618", GROUP="plugdev", MODE="0660"
  ACTION=="add", SUBSYSTEMS=="usb", ATTRS{idVendor}=="1618", RUN+="/bin/sh -c 'echo 200 > /sys/module/usbcore/parameters/usbfs_memory_mb || true'"
  ```

- `/usr/sbin/rusty-photon-qhy-firmware-install` (root-only, run manually;
  postinst prints a pointer but never downloads — offline installs must not
  fail): downloads the QHYCCD SDK pinned at **26.06.04** from
  `https://www.qhyccd.com/file/repository/publish/SDK/260604/` —
  `sdk_linux_arm64_26.06.04.tar.gz` (aarch64) or
  `sdk_linux64_26.06.04.tar.gz` (x86_64) — verifies the pinned sha256
  (pins live in the helper; the URL-dir scheme for >= 26.06.04 is the
  dotless YYMMDD form, matching `qhyccd-sdk-install@v4`), and installs the
  pieces a camera needs at runtime: firmware images → `/lib/firmware/qhy`,
  the SDK's `85-qhyccd.rules` → `/etc/udev/rules.d/` with its `RUN` paths
  rewritten, and QHYCCD's FX2/FX3-capable `fxload` → `/usr/local/sbin/fxload`
  (never `/usr/sbin/fxload`, which Debian's FX2-only `fxload` package may
  own). It installs neither `libqhyccd.so` (we link statically) nor
  headers/samples. `--force` re-installs; `--root DIR` supports container
  testing.

  **PR-4 finding — the helper installs the SDK udev rules + fxload after
  all** (this plan originally said firmware files only): on Linux the SDK
  performs **no in-process firmware upload** — its only firmware-path entry
  points (`OSXInitQHYCCDFirmware*`) are macOS/Android, and `libqhyccd.a`
  embeds firmware *basenames* but no search directory. A cold-plugged
  camera enumerates as a raw Cypress controller (VID `1618` with a raw
  per-model PID; the legacy Cypress VID `04b4` appears only in the bare
  FX2 dev-board rule) and receives firmware solely via the SDK's udev
  `RUN` rules exec'ing fxload as root out of `/lib/firmware/qhy`. Without
  those rules and QHYCCD's own fxload build (stock Debian fxload cannot
  program FX3), a factory-fresh camera never becomes usable.

### zwo-camera (PR-5)

- MIT blobs `libASICamera2.so` + `libEFWFilter.so` (from the same pinned
  indi-3rdparty commit `.github/actions/install-zwo-sdk` uses) are packaged
  at `/usr/lib/rusty-photon/`, license at
  `/usr/share/doc/rusty-photon-zwo-camera/ZWO-SDK-LICENSE.txt`. The license
  asset is a **committed copy** in `pkg/` (cargo-deb assets stay inside the
  crate dir); the checker `cmp`s it against the canonical
  `crates/zwo-rs/libzwo-sys/sdk/include/license.txt` so it cannot drift.
- The blobs carry **no SONAME** → loader resolution via **RUNPATH**:
  `build-packages.sh` exports
  `RUSTFLAGS="-C link-arg=-Wl,-rpath,/usr/lib/rusty-photon"` for the release
  build (uniform across binaries; harmless where unused; deliberately not a
  `build.rs` change, which would ripple into Bazel/repin). Expected lintian
  finding `custom-library-search-path` is documented, not fixed.
- deb `depends` are **explicit** (`libc6, libgcc-s1, libstdc++6,
  libusb-1.0-0, libudev1, adduser`) — dpkg-shlibdeps cannot map SONAME-less
  private blobs and `$auto` would fail.
- rpm mirrors that with `auto-req = "disabled"`: rpm's dependency generator
  would emit requires on the bundled SONAME-less blobs that no package
  provides, making the rpm uninstallable. The deb `depends` list stays the
  canonical statement of runtime needs (rpm is an x86_64 dev-box
  convenience pre-1.0).
- Build staging: blobs downloaded into gitignored
  `services/zwo-camera/pkg/lib/`; `ZWO_SDK_LIB_DIR` points there for the
  link; `cargo deb` picks them up as plain assets. The checker asserts (once
  `build-packages.sh` exists) that its `ZWO_SDK_REF` pin equals the
  `install-zwo-sdk` action's default ref — shipped blobs and CI-linked blobs
  must come from the same commit.
- Own udev rule `90-rusty-photon-zwo.rules` (VID `03c3` → `plugdev`/`0660`
  + the usbfs memory bump). ZWO cameras keep firmware in onboard flash — no
  upload step, no vendor udev rules, no firmware helper; the postinst
  stanza's firmware-pointer branch is a deliberate no-op here.

### phd2-guider (PR-3)

CLI package: binary asset (`/usr/bin/rusty-photon-phd2-guider`) only. No
unit, no user, no maintainer scripts beyond defaults.

**Update (2026-07-08, issue #464):** `phd2-guider` gained an HTTP service
mode (`serve`, the default when the binary runs with no subcommand) and is
now packaged like every other daemon: `pkg/` with the hardened unit +
canonical postinst/postrm, `systemd-units` deb metadata, shared
`rusty-photon` user. No `ConditionPathExists` gate — the service runs with
built-in defaults and merely reports 503 on `/health` until PHD2 is
reachable. The CLI subcommands remain available from the same binary.
`check-pkg-assets.sh`'s CLI-only exemption and `build-packages.sh`'s
explicit list append were removed with it.

### plate-solver note

ASTAP is an external runtime dependency (Recommends-level, not a hard dep):
the operator installs it separately (arm64 `.deb` from the ASTAP site) and
points the service config at the binary. To be documented in the operator
guide (`docs/packaging.md`, a PR-6 deliverable).

### On-device build — `scripts/build-packages.sh` (PR-6)

Runs on Debian arm64 (the rig) and x86_64 dev boxes:

1. apt prereqs (idempotent): `build-essential pkg-config curl git
   libusb-1.0-0-dev libudev-dev clang libclang-dev dpkg-dev
   ca-certificates` (`libssl-dev` and `libcfitsio-dev` from the original
   sketch dropped — the workspace links neither: TLS is rustls, FITS is
   pure-Rust `fitsrs`); `cargo install --locked cargo-deb`;
   `cargo-generate-rpm` only with `--rpm`.
2. Stage QHY SDK into `~/.cache/rusty-photon-pkg/` (same pinned URL + sha256
   as the firmware helper — the checker asserts version **and both archive
   sha256 pins** match), export `QHYCCD_SDK_DIR=<dir containing
   libqhyccd.a>` (located with `find`, not a hardcoded archive layout).
3. Stage ZWO blobs into `services/zwo-camera/pkg/lib/`, export
   `ZWO_SDK_LIB_DIR` to it.
4. `RUSTFLAGS="-C link-arg=-Wl,-rpath,/usr/lib/rusty-photon" cargo build
   --release -p <all service crates>`; strip binaries.
5. Per service: `cargo deb -p <crate> --no-build --no-strip` (`--no-build`
   is essential — a rebuild would lose the staged env/RUSTFLAGS). With
   `--rpm` on x86_64: `cargo generate-rpm -p services/<dir>`.
6. Collect into `dist/<version>/` + `SHA256SUMS.txt`.
7. Flags: `--services a,b,c` (subset), `--rpm`, `--skip-sdk-staging`
   (offline rebuild from cache).

## Verification

- `lintian` on all debs (documented expected findings:
  `custom-library-search-path` on zwo-camera; `no-changelog` /
  `no-manual-page` / `copyright-without-copyright-notice` accepted pre-1.0;
  `empty-field Depends` + `unstripped-binary` appear only on ad-hoc non-Debian
  host builds — Debian/CI builds run dpkg-shlibdeps and strip). All daemon
  packages carry `depends = "$auto, adduser"` (postinst uses adduser).
  `rpmlint` on rpms.
- **Rootless-container caveat (PR-2 finding):** rootless podman cannot apply
  the units' sandboxing (mount-namespace + seccomp setup across the `User=`
  switch fails with `217/USER` / `226/NAMESPACE`). `verify-packages.sh` must
  install a drop-in resetting the whole hardening block inside the container
  — packaging lifecycle is what containers verify; the hardening itself is
  verified on real hosts (`systemd-analyze security` + active unit on the
  rig).
- `scripts/verify-packages.sh` in a podman `--systemd=always` `debian:trixie`
  container (arm64 natively on the rig, x86_64 on the dev box), per service:
  install → unit `active` → config self-created at
  `/var/lib/rusty-photon/.config/rusty-photon/<svc>.json` with a minted
  `unique_id` → port probe (`/management/apiversions` for Alpaca services;
  service-specific endpoint otherwise) → remove (config survives) → purge
  (config + state gone; user/home/symlink stay).
  **PR-6 findings:** (1) the stock Debian container image ships a
  `policy-rc.d` that exits 101, silently blocking every maintainer-script
  unit start — the verify image removes it. (2) "unit active" is the wrong
  assertion for the five shared-transport serial drivers: their **eager
  startup handshake** exits non-zero when the device is absent (deliberate —
  never advertise a broken device), leaving the unit in a 5s restart loop
  until hardware appears. For them the script instead asserts: handshake
  attempted (journal), config self-created, and — only if a driver ever
  gains warn-and-serve — the port probe. The active+probe class is the
  cameras (qhy contract C0 warn-and-serve; zwo serves with zero cameras)
  plus the network-only daemons. (3) The cameras do **not** self-create a
  config (deliberate: no `materialize_identity` — UniqueIDs derive from
  camera serials; they run on defaults until `config.apply` or an operator
  writes a file), so the config-self-created assertion applies to the
  network-only daemons and serial drivers only.
- Cameras start with zero devices attached (qhy-camera contract C0:
  warn-and-serve). zwo-camera: `ldd` resolves `libASICamera2.so` to
  `/usr/lib/rusty-photon/` (RUNPATH proof).
- `config.apply` write-back on dsd-fp2 under the unit sandbox.
- Upgrade path: rebuild one service with `--deb-version 0.1.1-1`, install
  over: live config untouched, no prompts, unit restarted
  (`restart-after-upgrade`).
- `systemd-analyze security` once per class; record scores here.
- On-rig only: QHY firmware helper end-to-end with a real camera (download,
  sha256, replug, enumeration under plugdev/0660); serial access via
  dialout; Alpaca UDP discovery from another host.

### On-rig results (first install — 2026-07-05, Debian 13 arm64 rig)

- `build-packages.sh` from a fresh clone (only rustup + git pre-installed):
  15 arm64 debs + SHA256SUMS; the AARCH64 QHY sha256 pin verified on first
  use. On a Debian host `$auto` Depends resolve properly (qhy-camera:
  `adduser, libc6, libstdc++6, libusb-1.0-0`).
- All 15 installed in one apt transaction under the **real** hardening
  (no drop-ins): the 6 network/camera services active with 200 on their
  probes, the 5 serial drivers in the designed 5s retry loop, the 3 gated
  units inactive-not-failed; the 9 expected configs self-created;
  `/etc/rusty-photon` symlink in place; zwo blobs resolve via RUNPATH
  (`ldd`). The sandbox does not break config write-back.
- `systemd-analyze security`: network-only 6.2 MEDIUM (filemonitor),
  serial 6.9 MEDIUM (dsd-fp2), camera 6.9 MEDIUM (qhy-camera).
- QHY firmware helper end-to-end: download + sha256 + install OK; a cold
  QHY5III715C (raw Cypress `1618:0715`) flashed and re-enumerated as
  `1618:0716`, device node `plugdev`/`0660`, and the sandboxed service
  discovered and serves it (`cameras=1`). **Helper fix from this test:**
  a plain `udevadm trigger` emits change events, which do not fire the
  SDK rules' `ACTION=="add"` fxload lines — an already-plugged cold
  camera stayed raw; the helper now triggers
  `--action=add --subsystem-match=usb`.
- ZWO end-to-end with real hardware (ASI120MC-S): hot-plug →
  `90-rusty-photon-zwo.rules` applied `plugdev`/`0660` on the add event
  (no trigger needed) → `systemctl reload` (SIGHUP, no restart)
  re-enumerated → served on 11122 with a serial-derived UniqueID; the
  bundled MIT blobs do the USB work via the RUNPATH. Zero manual steps
  beyond the reload — the no-firmware-helper design confirmed.
- lintian (debian profile; filemonitor/qhy-camera/zwo-camera): only the
  findings documented in `docs/packaging.md` — no surprises.
- **Discovery: off by default is the intended behavior** (confirmed by
  Igor during this verification). Nothing binds UDP 32227: every service
  self-serves through rusty-photon-tls via `Server::into_service()`, which bypasses
  ascom-alpaca's discovery responder. That is correct for this family —
  rusty-photon puts up to 14 Alpaca servers on one host, which would
  collide on the discovery port (and a unicast query against a REUSEPORT
  group is answered by an arbitrary one of them). Clients are configured
  with explicit `host:port` instead. Follow-up landed as PR #450: the
  previously inert `discovery_port` config field is functional again as a
  per-host opt-in (`Option<u16>`, absent/`null` = disabled, the default —
  serialized only when set), restoring what PR #63 silently dropped when
  serving moved to `into_service()`.

### On-rig redeploy from merged main (2026-07-05)

After PR-6 (#448) and the discovery follow-up (#450) merged, the rig was
rebuilt from `main` and upgraded in place:

- `build-packages.sh` incremental rebuild (cargo cache from the branch
  build): 15 arm64 debs + SHA256SUMS.
- Same-version reinstall via `dpkg -i dist/0.1.0/*.deb` (apt skips an
  identical version; dpkg reinstalls) — all 15 unpacked `0.1.0-1` over
  `0.1.0-1`, maintainer scripts restarted every daemon, no prompts.
- Post-upgrade state identical to the first install: network/camera
  services active with probes answering (both cameras re-enumerated —
  QHY5III715C served on 11121, ZWO on 11122), serial drivers in the
  designed retry loop, gated units inactive-not-failed, configs untouched.
- The first-install caveat "old binaries' `config.apply` could re-persist
  a stale `discovery_port` key" is closed: the deployed binaries carry
  PR #450, which only serializes the field when set, and the rig configs
  carry no `discovery_port` keys. Discovery stays off on the rig (the
  intended default); the opt-in path is covered by
  `crates/rusty-photon-driver/tests/test_discovery.rs` and a pre-merge
  live smoke, and can be exercised on the rig any time by setting
  `server.discovery_port` and reloading.

## Flagged unknowns (resolve during PR-2/PR-4)

- [x] Firmware directory: confirmed `/lib/firmware/qhy` (every fxload `RUN`
      line in the SDK's udev rules hardcodes it, and the 26.06.04 archive
      ships `lib/firmware/qhy/**`; `libqhyccd.a` itself embeds only
      basenames).
- [x] In-process vs fxload: on Linux, firmware upload is **udev + fxload
      only** (the SDK's in-process entry points are macOS/Android-only), so
      the helper also installs the SDK's `85-qhyccd.rules` (RUN paths
      rewritten) and QHYCCD's fxload — see the PR-4 finding above. Cold
      cameras enumerate at VID `1618` with raw per-model PIDs; `04b4` is
      only the bare FX2 dev board, already covered by the SDK rules. The
      end-to-end cold-plug check stays an on-rig item (PR-6).
- [x] sha256 pins for both 26.06.04 archives captured in
      `rusty-photon-qhy-firmware-install` (single source of truth).
- [ ] `cargo-generate-rpm` support for a package `name` override (if
      missing: document and keep rpm crate-named until upstream supports it).
- [ ] `dirs`-crate home resolution under systemd `User=` without `$HOME`
      (units set `HOME` explicitly as belt-and-braces; confirm the fallback
      works anyway).

## Future considerations

- ~~`session-runner` postdates this plan's service inventory and is the one
  unpackaged daemon (network-only class, no new pattern needed). Package it
  in a small follow-up PR once the workflow-DSL implementation stabilizes.~~
  **Done (#699):** `services/session-runner/pkg/` carries the gated
  network-only unit + shared maintainer scripts, and its `Cargo.toml` the
  deb/rpm metadata; `services/*/pkg` discovery now packages it on every
  platform (config-gated, port 11171).
- PR-7: generalize `release.yml` to a service matrix (x86_64 deb/rpm).
  The rest of its original scope arrived via
  [nightly-releases.md](nightly-releases.md): version-parameterized
  `build-packages.sh` (N1), and the Homebrew rename + per-service formula
  generation + tap content (N4 — `release.yml`'s macOS/Homebrew jobs
  already use the shared scripts), leaving PR-7 the Linux matrix only.
- CI-built arm64 packages: designed in
  [nightly-releases.md](nightly-releases.md) (hosted `ubuntu-24.04-arm`
  runners; Orange Pi contingency settled by its N0 spike).
- An apt repository (and/or dnf copr) instead of GitHub-release attachments.
- `sky-survey-camera` and other simulators could later split into a
  `-simulators` meta-package if rig installs want to exclude them.

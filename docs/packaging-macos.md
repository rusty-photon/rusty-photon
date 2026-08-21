# macOS packaging & deployment guide

How to install, configure, and operate the rusty-photon services on a Mac.
macOS distribution is **Homebrew** — per-service binary formulas in the
[`ivonnyssen/homebrew-rusty-photon`](https://github.com/ivonnyssen/homebrew-rusty-photon)
tap, with `brew services` supervising the daemons the way systemd does on
Linux. The rationale (why not a suite `.pkg`) and the full design live in
[docs/plans/nightly-releases.md](plans/nightly-releases.md) (phase N4); the
native camera-SDK payload policy is
[ADR-013](decisions/013-native-sdk-payload-policy.md) /
[ADR-014](decisions/014-zwo-per-device-services-and-link-features.md). The
Linux guide is [docs/packaging.md](packaging.md), the Windows guide
[docs/packaging-windows.md](packaging-windows.md).

Apple Silicon (arm64) only — Intel macOS is not a target. Homebrew on
Linux is not supported either; Linux machines use the `.deb`/`.rpm`
packages.

## What gets installed

Each service is its own formula, `rusty-photon-<svc>`, installing
`rusty-photon-<svc>` into Homebrew's `bin` plus a `service do` block that
`brew services` turns into a managed launchd service. A meta-formula,
`rusty-photon`, depends on the whole family — the one-command install.
Nothing runs until you `brew services start` it, so install the family and
start only what the machine actually uses:

```sh
brew tap ivonnyssen/rusty-photon
brew install rusty-photon                      # the whole family
brew services start rusty-photon-zwo-camera    # start what this box uses
brew services start rusty-photon-ui-htmx
```

The tap really is `ivonnyssen/rusty-photon`, not the org slug: a tap name maps
to the repo `<owner>/homebrew-<name>`, and the formulas live in
`ivonnyssen/homebrew-rusty-photon` — a separate repository that stayed on the
personal account when this repo moved to the `rusty-photon` org. There is no
`rusty-photon/homebrew-rusty-photon`, so rewriting the tap breaks the install.

The services and their ports are the same family as on Linux (see the
table in [docs/packaging.md](packaging.md#what-gets-installed);
`session-runner` is likewise not yet packaged, and `svbony-camera` has no
macOS formula at all today — see "Camera specifics" below). The launchd
mapping:

| Linux concept | macOS equivalent |
|---|---|
| systemd unit, enabled + started on install | `service do` block; nothing starts until `brew services start` |
| `Restart=on-failure` + 5 s | `keep_alive true` (launchd respawn, ~10 s throttle) |
| `ConditionPathExists=` config gate | inherent — just don't start the service until its config exists |
| shared `rusty-photon` system user | your user (LaunchAgent); `sudo brew services` for a boot-time LaunchDaemon |
| journald | `$(brew --prefix)/var/log/rusty-photon-<svc>.log` |

Alpaca UDP discovery is deliberately not served (same reasoning as Linux);
point clients at `host:port` directly.

The sentinel formula additionally installs `rusty-photon-doctor`
(diagnosis, `--fix` repair, and the TLS + credential lifecycle —
[docs/services/doctor.md](services/doctor.md); there is no separate
doctor formula) and renders `rusty-photon-renew.plist` into its keg —
see [TLS renewal](#tls-renewal) below.

## Installing

```sh
brew tap ivonnyssen/rusty-photon
brew install rusty-photon              # meta-formula → every service
brew install rusty-photon-filemonitor  # or cherry-pick services
```

Start a service and verify it:

```sh
brew services start rusty-photon-filemonitor
curl http://localhost:11111/management/apiversions   # Alpaca services
```

`brew services start` registers the service to also start at login. On a
headless observatory Mac, use `sudo brew services start …` instead — that
installs a boot-time LaunchDaemon that runs without a user logged in (note
it then runs as root, so the config lives under root's home; see
Configuration below).

**Config-gated services** (`sky-survey-camera`, `plate-solver`,
`calibrator-flats`) have no sensible default config. There is no launchd
condition mechanism and none is needed: simply write the config first,
then `brew services start` it. Started without a config they exit
immediately and launchd respawn-loops them — harmless, but noisy.

**Serial-device drivers** (`ppba-driver`, `qhy-focuser`,
`pa-falcon-rotator`, `pa-scops-oag`, `dsd-fp2`, `star-adventurer-gti`)
validate their hardware eagerly at startup and exit if the device is
missing — by design, so a broken device is never advertised on the
network. Until the device is attached (and its path matches the config —
serial devices appear as `/dev/tty.usbserial-*` / `/dev/cu.*` on macOS),
launchd respawns the service every ~10 s; it comes up by itself once the
hardware appears. The cameras and the network-only services serve with no
hardware attached.

Because brew-installed binaries are downloaded by Homebrew's curl (no
quarantine xattr) and Rust's linker ad-hoc-signs arm64 binaries, nothing
here needs notarization or Gatekeeper approval.

## Nightly channel

Every service also has a `-nightly` sibling formula
(`rusty-photon-<svc>-nightly`, meta `rusty-photon-nightly`), regenerated by
CI on every publish of the rolling
[`nightly` prerelease](packaging.md#nightly-channel) and pointing at that
release's arm64 tarballs. The two channels conflict (same installed binary
names) — a machine follows one or the other:

```sh
brew install rusty-photon-nightly      # the family, nightly channel
brew update && brew upgrade            # pick up last night's build
```

Nightly formulas carry the full `<base>+nightly.<datetime>.g<sha>` version,
which Homebrew orders correctly: above the plain `<base>` release, below
the next patch release, monotonic across publishes. The stamp is UTC to
the minute and the numeric token does all the ordering (the `g<sha>`
token none); published stamps can never tie on the minute because
nightly runs serialize and each publish lands well after its stamp is
minted.

**Switching channels / rolling back.** `brew uninstall` the one channel's
formulas, then install the other (configs are untouched by uninstall).
The nightly channel keeps no history; to pin a known-good nightly, rebuild
that commit on a Mac with `scripts/build-tarballs.sh` and install the
binaries by hand, or wait out the next publish.

## Configuration

Formulas ship no config files. Daemons self-create their config on first
start at `~/Library/Application Support/rusty-photon/<svc>.json` — what
the shared config crate (`directories::ProjectDirs`) resolves on macOS;
there is no `~/.config` here and no `/etc/rusty-photon` symlink. Under
`sudo brew services` (LaunchDaemon, runs as root) the same path is under
root's home: `/var/root/Library/Application Support/rusty-photon/`.

The same class exceptions as Linux apply: the config-gated three never
write one, and the two cameras, `zwo-focuser`, and `phd2-guider` run on
built-in defaults without writing a file until settings are saved (via
ui-htmx `config.apply`) or one is created by hand.

To change settings, edit the file and restart (there is no `systemctl
reload` equivalent under `brew services`; a restart is always safe):

```sh
brew services restart rusty-photon-<svc>
```

`rp`'s self-created config defaults `session.data_directory` to
`~/Library/Application Support/rusty-photon/rp/data` — beside the config,
under the same platform root, since macOS has no systemd `StateDirectory=`
equivalent to provision the Linux `/var/lib/rusty-photon/rp/data`. rp
creates it at startup when it opens the target store, so the account
running the service must be able to write there; under `sudo brew
services` that resolves under root's home like the config does.

Caveat for configs written before this default was corrected: they carry
the unwritable Linux path verbatim, and rp now exits at startup with
`target_store.db_path ...: failed to create parent directory: Permission
denied`. Edit `session.data_directory` in
`~/Library/Application Support/rusty-photon/rp.json` to the path above and
restart.

**sentinel as watchdog**: sentinel discovers the installed
`rusty-photon-*` services from `brew services list` and derives each
restart as `brew services restart rusty-photon-<svc>` — nothing to
configure (see [sentinel.md §Service discovery](services/sentinel.md#service-discovery)).
brew has no `systemctl is-active` equivalent, so post-restart recovery
confirmation is skipped on macOS.

## First wiring: rusty-photon-doctor

The formulas put bytes on disk; doctor wires the configs (ADR-016).
After installing — or later adding — formulas, run once as the user the
services run as (under `sudo brew services`, prefix with `sudo`):

```sh
rusty-photon-doctor          # diagnose (exit 0 clean, 1 = findings)
rusty-photon-doctor --fix    # converge; a re-run shows the clean bill
```

Services pick fixed configs up on their next
`brew services restart rusty-photon-<svc>`.

### TLS renewal

A formula manages exactly one `brew services` daemon (sentinel itself),
so the daily renewal job ships as a plain launchd plist in sentinel's
keg. Arm it once — the formula's caveats print the same commands. The
symlink uses the upgrade-stable opt path, and it must land in
`~/Library/LaunchAgents` because launchd only auto-loads jobs from
there on login:

```sh
ln -sfv "$(brew --prefix rusty-photon-sentinel)/rusty-photon-renew.plist" ~/Library/LaunchAgents/
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/rusty-photon-renew.plist
```

(Headless installs running `sudo brew services` link into
`/Library/LaunchDaemons` and load the system domain instead:
`sudo launchctl bootstrap system /Library/LaunchDaemons/rusty-photon-renew.plist`.) The job
runs `rusty-photon-doctor tls renew` daily at 03:00 — a no-op until
certificates exist and are inside their renewal window. Running services
pick renewed certificates up without a restart (mtime-triggered
in-process reload). Unlike the Linux timer's `Persistent=true`, launchd
does not fire calendar jobs missed while powered off — a machine
habitually off at 03:00 should renew manually now and then
(`rusty-photon-doctor tls renew`); self-signed ten-year certificates
never practically need it.

## Camera specifics

**zwo-camera / zwo-focuser** — each formula's tarball bundles its own
MIT-licensed SDK dylib (`libASICamera2.dylib` / `libEAFFocuser.dylib`,
license installed into the formula's docdir), resolved keg-relative via
`@loader_path` rpaths (the macOS equivalent of the Linux packages'
RUNPATH; ADR-014's one-blob-per-service split unchanged). `zwo-camera`
additionally depends on Homebrew's `libusb`. No udev, no firmware
uploads — macOS needs no device-permission setup for USB *cameras*.

**zwo-focuser privacy grant** — the EAF is USB-HID, and its SDK dylib
touches macOS HID/Bluetooth frameworks that sit behind a privacy (TCC)
grant. Under `brew services` without one, the process blocks *pre-main*,
inside the dylib's static initializer (a stack sample shows it parked in
dyld's initializer phase) — alive, but silent: empty log, port never
bound. The same binary serves normally when run in a terminal. Grant
the binary access under System Settings → Privacy & Security (Input
Monitoring), or run `rusty-photon-zwo-focuser` once in a terminal to
trigger the prompt, then `brew services restart rusty-photon-zwo-focuser`.
The CI verify leg accordingly holds zwo-focuser to
alive-under-launchd + a foreground serve proof instead of a launchd port
probe; the exact grant UX is confirmed as part of the physical-Mac
validation pass.

**qhy-camera** — QHYCCD's proprietary SDK is linked statically (never
redistributed as files; ADR-013 is satisfied differently here: the mac SDK
embeds the camera firmware images *inside* the library). The formula
depends on Homebrew's `libusb`. **Caveat:** a factory-fresh ("cold") QHY
camera needs a one-time firmware upload. On Linux that is udev + fxload
(the firmware-install helper); on macOS the SDK instead exposes in-process
entry points (`OSXInitQHYCCDFirmware*`) which qhy-camera does not call
yet — so a cold camera will not enumerate on a Mac. A camera that has been
flashed since its last power-cycle (e.g. first plugged into a Linux host,
or previously used by another macOS app) works normally. Wiring the
in-process upload is a tracked follow-up.

**svbony-camera — not available on macOS today.** indi-3rdparty's
`libsvbony` packaging ships no `mac_arm64` blob (Intel/`mac64` only), and
Apple Silicon is the only Mac target this project builds for, so there is
no real SVBony SDK `build-tarballs.sh` could ever link against here.
Rather than publish a `SVBONY_SKIP_NATIVE_LINK=1` simulation-only binary as
the "real" tarball, `build-tarballs.sh`/`generate-brew-formulas.sh` exclude
`svbony-camera` outright — no `rusty-photon-svbony-camera` formula exists
in the tap. Revisit once a verified Apple Silicon blob exists (or SVBony
grants a redistribution license). See
[docs/services/svbony-camera.md](services/svbony-camera.md#packaging) for
the full reasoning.

## plate-solver: ASTAP

Same as Linux: ASTAP is an external runtime dependency — install the
macOS build (+ a star database) from the
[ASTAP site](https://www.hnsky.org/astap.htm) and point
`astap_binary_path` / `astap_db_directory` in the config at it.

## Logs

`brew services` routes stdout/stderr to
`$(brew --prefix)/var/log/rusty-photon-<svc>.log` (one file per service,
both streams).

## Removing

```sh
brew services stop rusty-photon-<svc>
brew uninstall rusty-photon-<svc>          # or the whole family:
brew uninstall rusty-photon $(brew list --formula | grep '^rusty-photon-')
```

Homebrew never touches runtime-created state (the rpm-erase parity, not
dpkg purge): to fully clean up, delete
`~/Library/Application Support/rusty-photon/` by hand.

## Verifying a build

On a Mac with Rust and Homebrew installed:

```sh
scripts/build-tarballs.sh                 # per-service arm64 tarballs into dist/<version>/
scripts/verify-brew.sh                    # full Homebrew lifecycle against them
scripts/verify-brew.sh --services filemonitor,zwo-camera --keep
```

`build-tarballs.sh` stages the pinned native SDKs (QHYCCD static lib for
the link; per zwo service its one MIT dylib, which becomes that tarball's
payload) and release-builds with the `@loader_path` rpaths — the zwo
services each in their own cargo invocation, so feature unification cannot
re-union their per-device SDK links. `svbony-camera` is skipped entirely
(no confirmed `mac_arm64` SVBony SDK blob — see "Camera specifics" above).
`verify-brew.sh` renders the formulas
with `file://` URLs into a scratch tap and, per service: install →
`brew test` → `brew services start` → HTTP probe → config self-created →
stop → uninstall clean — with the same class exceptions as
`verify-packages.sh` (gated services are never started; serial drivers
verify config + handshake-attempted from the log; the zwo binaries prove
via `otool` that each loads exactly its own bundled dylib). The nightly
`macos` leg runs exactly this pair as its pre-publish gate
(`.github/workflows/nightly-packages.yml`), as does `release.yml` for the
stable channel.

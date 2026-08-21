# Packaging & deployment guide

How to build, install, and operate the rusty-photon `.deb` / `.rpm`
packages on an observatory machine. Architecture decisions live in
[ADR-012](decisions/012-service-packaging-architecture.md) (naming, config
model, shared user, unit classes) and
[ADR-013](decisions/013-native-sdk-payload-policy.md) (native camera-SDK
payloads); the full design in
[docs/plans/service-packaging.md](plans/service-packaging.md); the
maintainer-script invariants in [packaging/README.md](../packaging/README.md).

Deployment is native packages by explicit decision — the drivers' USB /
udev / firmware needs and ASCOM Alpaca's UDP discovery defeat containers
(ADR-012).

Windows ships as one suite MSI instead of per-service packages — see
[docs/packaging-windows.md](packaging-windows.md) (ADR-015). macOS ships
per-service Homebrew formulas from the `rusty-photon/homebrew-rusty-photon`
tap — see [docs/packaging-macos.md](packaging-macos.md).

## What gets installed

Every package is named `rusty-photon-<svc>` and installs
`/usr/bin/rusty-photon-<svc>` plus a hardened
`rusty-photon-<svc>.service` unit that is enabled and started on install.
All daemons run as the shared system user `rusty-photon` (home
`/var/lib/rusty-photon`, no login shell), created by the first package
installed. (`phd2-guider` was originally the one plain CLI package; it
gained a unit when its HTTP service mode landed — issue #464. Its binary
doubles as the PHD2 CLI via subcommands.)

| Service | Port | Notes |
|---------|------|-------|
| filemonitor | 11111 | Alpaca SafetyMonitor |
| ppba-driver | 11112 | serial (dialout; deb adds plugdev) |
| qhy-focuser | 11113 | serial (dialout; deb adds plugdev) |
| sentinel | 11114 | dashboard: `/` |
| rp | 11115 | orchestrator API |
| sky-survey-camera | 11116 | config-gated (see below) |
| star-adventurer-gti | 11117 | serial (dialout; deb adds plugdev) |
| pa-falcon-rotator | 11118 | serial (dialout; deb adds plugdev) |
| dsd-fp2 | 11119 | serial (dialout; deb adds plugdev) |
| ui-htmx | 11120 | web config UI |
| qhy-camera | 11121 | USB camera; needs the firmware helper (below) |
| zwo-camera | 11122 | USB camera; its SDK blob bundled |
| pa-scops-oag | 11123 | serial (dialout; deb adds plugdev) |
| zwo-focuser | 11124 | USB focuser; its SDK blob bundled |
| planetarium-bridge | 11126 | virtual planetarium target-entry telescope (no hardware) |
| phd2-guider | 11130 | guider service wrapping PHD2 (PHD2 installed separately, below) |
| plate-solver | 11131 | config-gated; needs ASTAP (below) |
| calibrator-flats | 11170 | config-gated |
| session-runner | 11171 | config-gated |
| polar-align | 11172 | config-gated |

Alpaca UDP discovery is deliberately not served: with this many Alpaca
servers on one host they would collide on the discovery port. Point
clients (N.I.N.A. etc.) at `host:port` directly using the table above.

The sentinel package additionally carries the operator tool
`/usr/bin/rusty-photon-doctor` (diagnosis, `--fix` repair, and the TLS +
credential lifecycle — [docs/services/doctor.md](services/doctor.md))
plus the TLS renewal units `rusty-photon-renew.service` /
`rusty-photon-renew.timer`. There is no separate doctor package:
sentinel is the always-installed supervisor, so its artifact is the
delivery vehicle (ADR-016).

## Building packages

Packages are built natively on the target architecture — nightly in CI on
hosted x86_64 + arm64 runners (see [Nightly channel](#nightly-channel)),
or on demand directly on the rig / a dev box. From a repo checkout on a
Debian-family machine with Rust installed:

```sh
scripts/build-packages.sh                  # all services, .deb only
scripts/build-packages.sh --rpm            # also .rpm
scripts/build-packages.sh --services qhy-camera,filemonitor
scripts/build-packages.sh --skip-sdk-staging   # offline rebuild from cache
scripts/build-packages.sh --deb-version 0.1.0+nightly.202607120507.gba09dc9
                                           # nightly version stamp (CI / rollback builds)
```

With `--rpm` and `--deb-version` together, the rpm version is derived
from the deb stamp by rendering `+nightly.` as rpm's `^` snapshot
separator (`0.1.0^202607120507.gba09dc9`) — each packager renders its own
dialect of "sorts after the base release, before the next one".

The script installs apt build prerequisites, stages the pinned native
SDKs into `~/.cache/rusty-photon-pkg/` (QHYCCD static lib for the
link; per zwo service its ONE MIT blob, which also becomes that package's
payload per ADR-013 + ADR-014), then release-builds with the RUNPATH the
zwo packages need — the two zwo services each in their own cargo
invocation, so feature unification cannot re-union their per-device SDK
links — and runs `cargo deb` (and `cargo generate-rpm` with `--rpm`) per
service. Artifacts land in `dist/<version>/` with a `SHA256SUMS.txt`.

The QHY SDK version/sha256 pins and the ZWO blob ref are pinned in the
script and cross-checked by `scripts/check-pkg-assets.sh` against the
firmware helper and the CI SDK action, so shipped and CI-linked SDK bits
cannot drift apart.

## Nightly channel

CI publishes a rolling **`nightly` prerelease** built from the HEAD of
`main` whenever it has changed since the last publish
(`.github/workflows/nightly-packages.yml`): every packaged service as a
`.deb` *and* `.rpm` for both amd64/x86_64 and arm64/aarch64, each
package lifecycle-verified in a systemd container (Debian for the debs,
Fedora for the rpms) before anything is published, plus the Windows
suite MSI ([docs/packaging-windows.md](packaging-windows.md#nightly-channel))
and the macOS arm64 tarballs with their regenerated `-nightly` Homebrew
formulas ([docs/packaging-macos.md](packaging-macos.md#nightly-channel))
— all-or-nothing across the legs, so the release is always one coherent
commit with a complete asset set. There is one release and one tag;
assets are replaced on each publish, with no dated history. The same
debs and rpms are additionally published as a real `apt`/`dnf`
repository (see [Package repositories](#package-repositories-recommended)
below), which is the recommended way to consume the channel on Linux.

Nightly debs carry the version `<base>+nightly.<datetime>.g<sha>` (e.g.
`0.1.0+nightly.202607120507.gba09dc9`, UTC to the minute), which dpkg
sorts above the plain `<base>` release and below the next patch release —
`apt` upgrades a release install to a nightly in place, and the next
release upgrades over any nightly. The stamp carries the time, not just
the date, because it is the only ordered part of the version: the
`g<sha>` suffix compares as hex, so a second publish on the same day
must out-sort the first on the timestamp alone.

Nightly rpms carry `<base>^<datetime>.g<sha>` (e.g.
`0.1.0^202607120507.gba09dc9`); rpm's `^` separator sorts the same way,
so `dnf` upgrades in place identically. One wrinkle: GitHub rewrites `^`
to `.` in uploaded asset names, so the *file* is called
`…-0.1.0.<datetime>.g<sha>-1.<arch>.rpm` while `rpm -q` after install
shows the true `^` version. `SHA256SUMS.txt` lists the dot-rendered names, so
checksums verify against the files as downloaded.

### Package repositories (recommended)

The channel is also served as a real `apt`/`dnf` repository at
`pkg.rustyphoton.space` (a Cloudflare R2 public bucket;
tools/rusty-photon-packages-r2/README.md documents the hosting), so a
machine set up once picks every nightly up with plain
`apt upgrade` / `dnf upgrade`. The repository is rolling-only — exactly
one version, the current nightly; the GitHub release assets below stay
as the manual path (and the downgrade/rollback path).

Clients verify the repo metadata against the signing key; its
fingerprint is

```
C2BE 1E02 D49E 111B 6BEC  2882 51AA 3DE5 44C0 0B8F
```

(the same key as `packaging/gpg/pubkey.asc` in this repo — compare
before trusting the downloaded copy).

Debian-family:

```sh
sudo install -d /etc/apt/keyrings
sudo curl -fsSLo /etc/apt/keyrings/rusty-photon.asc https://pkg.rustyphoton.space/pubkey.asc
echo "deb [signed-by=/etc/apt/keyrings/rusty-photon.asc] https://pkg.rustyphoton.space/deb nightly main" \
    | sudo tee /etc/apt/sources.list.d/rusty-photon-nightly.list
sudo apt update
sudo apt install rusty-photon-<svc>     # thereafter: plain `apt upgrade`
```

Fedora:

```sh
sudo tee /etc/yum.repos.d/rusty-photon-nightly.repo <<'EOF'
[rusty-photon-nightly]
name=Rusty Photon nightly
baseurl=https://pkg.rustyphoton.space/rpm/$basearch/
enabled=1
repo_gpgcheck=1
gpgcheck=0
gpgkey=https://pkg.rustyphoton.space/pubkey.asc
EOF
sudo dnf install rusty-photon-<svc>     # thereafter: plain `dnf upgrade`
```

`repo_gpgcheck=1` is what makes dnf verify the repo signature at all
(dnf imports the key on first contact — check the fingerprint it shows
against the one above); `gpgcheck=0` because individual packages carry
no signature — the signed metadata's checksums cover them.

### Manual asset download

Filenames change nightly (they carry the version), so use
`SHA256SUMS.txt` — the one asset with a stable URL — as the index:

```sh
curl -fsSL https://github.com/rusty-photon/rusty-photon/releases/download/nightly/SHA256SUMS.txt
# pick the file for your service + arch, then:
curl -fLO "https://github.com/rusty-photon/rusty-photon/releases/download/nightly/<file>"
sha256sum -c --ignore-missing SHA256SUMS.txt
sudo apt-get install "./<file>"     # Debian-family
sudo dnf install "./<file>"         # Fedora
```

or, with the GitHub CLI (rpms: `--pattern 'rusty-photon-<svc>-*.<arch>.rpm'`
with `<arch>` = `x86_64` or `aarch64`):

```sh
gh release download nightly --repo rusty-photon/rusty-photon \
    --pattern 'rusty-photon-<svc>_*_arm64.deb'
sudo apt-get install ./rusty-photon-<svc>_*_arm64.deb
```

Upgrading is installing a newer nightly the same way; a running unit is
restarted onto the new binary and the config untouched, as with any
package upgrade.

**Downgrades.** Once a machine runs nightlies, anything older is a
downgrade — an on-demand build stamped with the plain workspace
version, or an older nightly — and needs:

```sh
sudo apt-get install --allow-downgrades ./rusty-photon-<svc>_0.1.0-1_arm64.deb
sudo dnf downgrade ./rusty-photon-<svc>-0.1.0-1.<arch>.rpm      # Fedora
```

**Rolling back.** The channel keeps no history. To return to a
known-good state, downgrade to the plain release as above, or rebuild
the known-good commit on demand (add `--rpm` for the rpm set) and
install that the same downgrade way:

```sh
git checkout <known-good-sha>
scripts/build-packages.sh --deb-version "<base>+nightly.<datetime>.g<short-sha>"
```

(`<base>` = the workspace version at that commit.)

## Installing

```sh
sudo apt-get install ./rusty-photon-<svc>_*.deb
```

`apt-get install ./<file>` (not `dpkg -i`) resolves the runtime
dependencies. The unit is enabled and started immediately; on upgrade it is
restarted. On Fedora:

```sh
sudo dnf install ./rusty-photon-<svc>-*.rpm
sudo systemctl start rusty-photon-<svc>
```

The rpm enables the unit but — Fedora convention — does not start it:
start it once by hand (or reboot); upgrades restart a running unit and
leave a stopped one alone. Verify with:

```sh
systemctl status rusty-photon-<svc>
curl http://localhost:<port>/management/apiversions   # Alpaca services
```

**Config-gated services** (`sky-survey-camera`, `plate-solver`,
`calibrator-flats`, `session-runner`, `polar-align`) have no sensible default config, so their units carry
`ConditionPathExists=` on the config file: on a fresh install the unit
stays inactive (not failed) until you write
`/etc/rusty-photon/<svc>.json`, then `systemctl start rusty-photon-<svc>`.

**Serial-device drivers** (`ppba-driver`, `qhy-focuser`,
`pa-falcon-rotator`, `pa-scops-oag`, `dsd-fp2`, `star-adventurer-gti`) validate their
hardware eagerly at startup and exit if the device is missing — by design,
so a broken device is never advertised on the network. Until the device is
attached (and its path matches the config), the unit sits in a
restart-every-5s loop; it comes up by itself once the hardware appears.
The cameras and the network-only services serve with no hardware attached.

## First wiring: rusty-photon-doctor

The installer puts bytes on disk; doctor wires the configs (ADR-016).
After installing — or later adding — packages, run once:

```sh
sudo rusty-photon-doctor          # diagnose (exit 0 clean, 1 = findings)
sudo rusty-photon-doctor --fix    # converge; a re-run shows the clean bill
```

`--fix` repairs what the checks flag (retired config keys, missing
joins, fixable shapes) and — see
[doctor.md §Provisioning](services/doctor.md) — can generate TLS
material and mint + distribute the observatory credential for every
installed service. Running it while services are live is fine
(warn-and-proceed, atomic writes); services pick fixed configs up on
their next restart: `sudo systemctl restart 'rusty-photon-*'`.
Everything doctor writes under sudo is chowned back to the
`rusty-photon` user, pki material included.

### TLS renewal

The sentinel package ships `rusty-photon-renew.timer` (daily, jittered,
`Persistent=true`), running `rusty-photon-doctor tls renew` as the
`rusty-photon` user — a no-op until certificates exist and are inside
their renewal window, so it is safe armed on every install. The deb
starts the timer on install; the rpm (Fedora convention) enables it to
arm on the next boot, or start it once by hand:

```sh
sudo systemctl start rusty-photon-renew.timer
systemctl list-timers rusty-photon-renew.timer   # shows the next fire
```

Running services pick renewed certificates up without a restart
(mtime-triggered in-process reload; ADR-002). Sentinel's watchdog
deliberately ignores the renew unit — it is a scheduled job, not a
daemon to supervise.

## Configuration

Packages ship no config files. Daemons self-create their config on first
start at `/var/lib/rusty-photon/.config/rusty-photon/<svc>.json` (the
shared user's XDG path), reachable via the `/etc/rusty-photon` symlink.
Exceptions: the config-gated five (above) never write one. To change
settings:

```sh
sudo -e /etc/rusty-photon/<svc>.json
sudo systemctl reload rusty-photon-<svc>    # reload-capable services
sudo systemctl restart rusty-photon-<svc>   # the rest
```

Reload-capable (SIGHUP): filemonitor, ppba-driver, qhy-focuser,
sky-survey-camera, pa-falcon-rotator, pa-scops-oag, dsd-fp2,
star-adventurer-gti, qhy-camera, zwo-camera. Note that services with `config.apply` support
(via ui-htmx) rewrite these files at runtime — hand-edits and UI edits
share the same file.

## Sentinel restart privileges (polkit)

Sentinel's restart endpoint, watchdog ladder, and health supervision shell
out to `systemctl restart rusty-photon-<svc>` as the unprivileged
`rusty-photon` user — its unit sets `NoNewPrivileges=yes`, so a `sudo`
prefix could never work. The sentinel package
therefore ships a scoped polkit rule,
`/usr/share/polkit-1/rules.d/50-rusty-photon-sentinel.rules`, granting that
user exactly the `restart` verb on `rusty-photon-*` units; other verbs and
non-prefixed units still require the usual authorization. polkitd picks the
rule up on install with no reload step. The restart commands themselves are
derived from the discovered unit names — the rule's scope and the discovery
scope are the same set (see
[sentinel.md §Service discovery](services/sentinel.md#service-discovery)).

## Camera specifics

**qhy-camera** — QHYCCD's SDK is proprietary and never redistributed
(ADR-013). After installing the package, run once, as root, with internet
access:

```sh
sudo rusty-photon-qhy-firmware-install
```

It downloads the pinned SDK release from qhyccd.com, verifies a pinned
sha256, and installs the camera firmware images, QHYCCD's udev
firmware-upload rules, and their FX3-capable `fxload`. An already-plugged
cold camera is flashed immediately (the helper re-emits udev add events);
otherwise firmware uploads on the next plug-in. Offline installs work; the
camera just stays unusable until the helper has run.

**zwo-camera / zwo-focuser** — nothing to do: each package bundles its own
MIT-licensed SDK blob at `/usr/lib/rusty-photon/` (`libASICamera2.so` /
`libEAFFocuser.so`; license in the package docdir), so the two co-install
cleanly (ADR-014). ZWO devices keep firmware in onboard flash.

**svbony-camera** — SVBony's SDK carries no license grant at all (ADR-018),
so unlike ZWO it is never bundled. After installing the package, run once,
as root, with internet access:

```sh
sudo rusty-photon-svbony-sdk-install
```

It downloads the pinned SDK library from the same indi-3rdparty commit CI
links against, verifies a pinned sha256, and installs `libSVBCameraSDK.so`
to `/usr/lib/rusty-photon/` (the package's binary is linked with the
matching RUNPATH, mirroring `zwo-camera`'s mechanism). Offline installs
work; the camera just stays unusable until the helper has run.

Until it has, the service cannot load the SDK and systemd keeps retrying it
(a loader error every few seconds in `journalctl -u
rusty-photon-svbony-camera`) — that retry is the recovery path: the service
starts serving within seconds of the helper finishing, with no `systemctl`
step needed.

The same helper is the supported bootstrap on a machine with **no**
`rusty-photon-svbony-camera` package (a developer box running the binary
from a checkout): run it from the source tree
(`sudo services/svbony-camera/pkg/rusty-photon-svbony-sdk-install`) and it
installs a udev rule for VID `f266` alongside the SDK, since nothing else
would grant USB access there. It never touches udev when the packaged rule
is already installed; `--no-udev` opts out entirely and `--udev-group
NAME` picks the owning group. See
[docs/services/svbony-camera.md](services/svbony-camera.md#udev--usb).

Every camera package installs a udev rule assigning their USB VID's
device nodes to the `rusty-photon` service group (the account's own
primary group — no supplementary groups needed, and no reliance on
Debian's `plugdev`, which rpm-family hosts lack).

## plate-solver: ASTAP

ASTAP is an external runtime dependency, deliberately not a package
dependency (bring-your-own, [ADR-005](decisions/005-plate-solver.md)):
you install the solver and a star database yourself and point the
service's config at them. The service is config-gated — the packaged
unit stays inert until `plate-solver.json` exists.

1. **Install the solver binary.** The wrapper drives `astap_cli`, the
   command-line solver — the GUI program is not needed. Download the
   zip for your architecture from the
   [ASTAP downloads](https://www.hnsky.org/astap.htm) (SourceForge
   `linux_installer/`, e.g.
   `astap_command-line_version_Linux_aarch64.zip` on a Pi,
   `…_amd64.zip` on x86_64), then:

   ```sh
   unzip astap_command-line_version_Linux_*.zip
   sudo install -m 755 astap_cli /usr/local/bin/astap_cli
   ```

2. **Install a star database.** Upstream's own rule: with a field of
   view of 0.6° or larger, any of D05/D20/D50/D80 works — they are all
   Gaia-derived to a similar depth, at increasing star density (and
   size: D05 ≈ 100 MB up to D80 ≈ 1.25 GB). D05 is plenty for typical
   deg-class refractor fields (the reference rig's 360 mm + IMX178
   ≈ 1.2° × 0.8° solves with it; it is also what CI pins). Go denser
   (D50/D80) only below ~0.6°, and to W08 for very wide fields
   (> 20°). Either install the database `.deb` from the same site
   (confirm where it lands with `dpkg -L`) or unzip the database zip
   into a directory of your choice. Whichever way, `astap_db_directory`
   must point at the *specific* directory holding that database's
   files — the examples here and in the
   [service docs](services/plate-solver.md) use `/opt/astap/d05`.

3. **Write the config.** Create
   `/etc/rusty-photon/plate-solver.json` (both keys are mandatory —
   there is no built-in default, which is exactly why the unit gates
   on the file):

   ```json
   {
     "server": { "port": 11131 },
     "astap_binary_path": "/usr/local/bin/astap_cli",
     "astap_db_directory": "/opt/astap/d05"
   }
   ```

   The file (and both paths) must be readable by the `rusty-photon`
   user — the service validates them at startup and again on every
   `/health` probe. See
   [docs/services/plate-solver.md](services/plate-solver.md) for the
   full config surface (timeouts, concurrency, hints).

4. **Start and verify.**

   ```sh
   sudo systemctl start rusty-photon-plate-solver
   curl -s -w '\n%{http_code}\n' http://127.0.0.1:11131/health
   ```

   `200` with `{"status":"ok"}` means binary and database both check
   out; `503` carries `{"status":"binary_unavailable"}` or
   `{"status":"db_unavailable"}`, naming the check that failed, plus
   a `message` with the offending path. Sentinel shows a `503` as
   *degraded* (amber, message displayed) rather than restart-looping
   a service whose problem a restart cannot cure.

## phd2-guider: PHD2

PHD2 is an external runtime dependency, deliberately not a package
dependency (bring-your-own, same posture as ASTAP above): `phd2-guider`
connects to an already-running PHD2 on `localhost:4400` and never spawns
it — the operator owns the PHD2 process (see
[phd2-guider.md §HTTP Service Mode](services/phd2-guider.md#http-service-mode-serve)).
The service itself needs no setup: it runs on built-in defaults and
keeps retrying the PHD2 connection in the background, so the install
order does not matter and no service restart is ever needed.

Debian and Raspberry Pi OS ship no `phd2` package, so on a rig host you
build it from source (upstream's supported route — minutes, not hours,
on a Pi 5 class machine). PHD2 is a GUI application, so a headless host
also needs a persistent virtual display; TigerVNC provides that and
remote viewing in one.

1. **Build and install PHD2.**

   ```sh
   sudo apt-get install -y build-essential git cmake pkg-config \
     libwxgtk3.2-dev wx-common wx3.2-i18n gettext zlib1g-dev libx11-dev \
     libcurl4-gnutls-dev libcfitsio-dev libnova-dev libusb-1.0-0-dev \
     libeigen3-dev libopencv-dev libgtest-dev
   git clone --depth 1 --branch v2.6.14 https://github.com/OpenPHDGuiding/phd2.git
   cmake -S phd2 -B phd2/tmp -DCMAKE_BUILD_TYPE=Release
   make -C phd2/tmp -j"$(nproc)"
   sudo make -C phd2/tmp install  # lands at /usr/bin/phd2 (PHD2's cmake sets the /usr prefix itself)
   ```

   Do **not** pass `-DUSE_SYSTEM_LIBINDI=1` on Debian or Raspberry Pi
   OS: their `libindi-dev` is 1.9.x and PHD2 requires INDI ≥ 2.0, so
   the default configure — which downloads and builds PHD2's own INDI
   client — is the working path. The default build also includes the
   bundled vendor camera backends (QHY, ZWO, and friends), so guide
   cameras connect natively with no INDI server involved.

2. **Run PHD2 headless under TigerVNC.**

   ```sh
   sudo apt-get install -y tigervnc-standalone-server
   tigervncpasswd                     # one-time: set the viewing password
   printf '#!/bin/sh\nexport GDK_BACKEND=x11\nexec phd2\n' > ~/.vnc/xstartup
   chmod +x ~/.vnc/xstartup
   tigervncserver :1 -localhost yes -geometry 1280x800
   ```

   PHD2 *is* the VNC session: quitting PHD2 ends the session, and
   `tigervncserver -kill :1` is the other way down. The
   `GDK_BACKEND=x11` line is load-bearing — on a host where any Wayland
   session exists (a Raspberry Pi OS console, for example), GTK prefers
   the Wayland socket over `DISPLAY` and PHD2 would come up invisible
   on the physical console instead of the VNC display. The password
   tool must be `tigervncpasswd`: on Raspberry Pi OS plain `vncpasswd`
   is RealVNC's unrelated utility and writes a password file TigerVNC
   cannot read. If `tigervncserver` reports the server "cleanly exited"
   right after starting, PHD2 crashed on launch (`~/.vnc/<host>:1.log`
   shows it) — a one-off on the very first start after installation has
   been observed; starting the session again recovers.

3. **Create the equipment profile (first run only).** Tunnel the
   display to your workstation and point any VNC viewer at
   `localhost:5901`:

   ```sh
   ssh -L 5901:127.0.0.1:5901 <rig-host>
   ```

   Complete PHD2's first-light wizard. Equipment choices are
   site-specific and out of scope here; the built-in camera/mount
   simulator makes a fine smoke-test profile. Afterwards quit PHD2
   cleanly once (File → Exit — this ends the VNC session) and start it
   again with the `tigervncserver` line above: PHD2 persists its
   profile to `~/.PHDGuidingV2` on clean exit, not while running, and
   an unsaved profile would be lost to a power cut.

4. **Verify.**

   ```sh
   curl -s -w '\n%{http_code}\n' http://127.0.0.1:11130/health
   ```

   `200` with `{"status":"ok"}` means phd2-guider holds a live
   connection to PHD2. `503` with `{"status":"unavailable"}` (plus a
   human-readable `message`) means it does not — PHD2 is not running,
   or its event server is disabled (PHD2's Tools → Enable Server, a
   per-profile setting that defaults to on). The service reconnects on
   its own within seconds of PHD2 appearing.

Like every other daemon (see [Configuration](#configuration)),
phd2-guider self-creates its config on first start, which also gives
sentinel a health-probe URL to derive — the guider is supervised from
then on. While PHD2 is off — the normal daytime state of a rig — the
guider's `503` counts as **alive but degraded** under
[sentinel's health supervision](services/sentinel.md#service-health-supervision):
no restarts, no notifications, an amber dashboard row showing the
guider's own `message` (issue #595). Supervision still restarts the
guider the moment it stops answering HTTP at all.

## Removing

```sh
sudo apt-get remove rusty-photon-<svc>   # keeps the service's config + state
sudo apt-get purge rusty-photon-<svc>    # also deletes its config + state dir
```

The shared user, `/var/lib/rusty-photon`, and the `/etc/rusty-photon`
symlink are never removed (shared across packages, Debian convention for
system users). rpm has no purge lifecycle: erase behaves like `remove`;
to fully clean up after an erase, delete
`/var/lib/rusty-photon/.config/rusty-photon/<svc>.json` and
`/var/lib/rusty-photon/<svc>/` by hand.

## Verifying a build

```sh
scripts/verify-packages.sh            # all debs in dist/<version>/
scripts/verify-packages.sh --rpm      # the rpms, in a Fedora container
scripts/verify-packages.sh --services filemonitor,zwo-camera --keep
```

Runs a podman `--systemd=always` debian:trixie container and, per package:
install → unit active → config self-created → HTTP probe → remove (config
survives) → purge (config and state gone, shared pieces stay). The `--rpm`
flavor runs the same per-service checks in a Fedora container, adjusted
where rpm's lifecycle genuinely differs: it asserts the scriptlets'
enabled-but-not-started contract before starting each unit itself, its
`dnf install` doubles as the proof that every rpm's declared requires
resolve (nothing is preinstalled to compensate), and erase is verified as
remove-not-purge — config and state must survive. Gated
services verify enabled-but-inactive-and-not-failed instead; zwo-camera
additionally proves via `ldd` that each zwo binary resolves exactly its own
bundled blob through the RUNPATH — and does not link the other services'
SDKs (ADR-014). Rootless podman cannot apply the units' sandboxing, so
the script resets the hardening inside the container — hardening is
verified on real hosts with `systemd-analyze security
rusty-photon-<svc>.service`.

Expected `lintian` findings (accepted, not bugs):
`custom-library-search-path` on every package (the RUNPATH is injected
uniformly; only the zwo packages use it); `no-changelog` / `no-manual-page` /
`copyright-without-copyright-notice` pre-1.0; `unstripped-binary-or-object`
and `hardening-no-relro` on the vendored ZWO blobs (shipped exactly as
published); `embedded-library` on qhy-camera's statically linked SDK;
`appstream-metadata-missing-modalias-provide` on the camera packages' udev
rules; `empty-field Depends` and `unstripped-binary` on our own binaries
only on ad-hoc builds from non-Debian hosts.

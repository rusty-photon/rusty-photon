# Packaging & deployment guide

How to build, install, and operate the rusty-photon `.deb` / `.rpm`
packages on an observatory machine. Architecture decisions live in
[ADR-012](decisions/012-service-packaging-architecture.md) (native
packages, naming, config model, shared user, unit classes) and
[ADR-013](decisions/013-native-sdk-payload-policy.md) (native camera-SDK
payloads); the maintainer-script invariants in
[packaging/README.md](../packaging/README.md).

Deployment is native packages: the drivers' USB / udev / firmware needs
defeat containers.

Windows ships as one suite MSI instead of per-service packages — see
[docs/packaging-windows.md](packaging-windows.md). macOS ships
per-service Homebrew formulas from the `rusty-photon/homebrew-rusty-photon`
tap — see [docs/packaging-macos.md](packaging-macos.md).

## What gets installed

Every package is named `rusty-photon-<svc>` and installs
`/usr/bin/rusty-photon-<svc>` plus a hardened
`rusty-photon-<svc>.service` unit that is enabled and started on install.
All daemons run as the shared system user `rusty-photon` (home
`/var/lib/rusty-photon`, no login shell), created by the first package
installed.

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
| ui-htmx | 11120 | web UI: activity stream, targets inbox |
| qhy-camera | 11121 | USB camera; needs the firmware helper (below) |
| zwo-camera | 11122 | USB camera; its SDK blob bundled |
| pa-scops-oag | 11123 | serial (dialout; deb adds plugdev) |
| zwo-focuser | 11124 | USB focuser; its SDK blob bundled |
| svbony-camera | 11125 | USB camera; its SDK installed on the target by a helper (below) |
| planetarium-bridge | 11126 | virtual planetarium target-entry telescope (no hardware) |
| upbv2-driver | 11127 | serial (dialout; deb adds plugdev) |
| gemini-powerbox-adv3-driver | 11132 | serial (dialout; deb adds plugdev) |
| plate-solver | 11131 | config-gated; needs ASTAP (below) |
| calibrator-flats | 11170 | config-gated |
| session-runner | 11171 | config-gated |
| polar-align | 11172 | config-gated |
| focus-model | 11173 | config-gated |

Alpaca UDP discovery is deliberately not served: with this many Alpaca
servers on one host they would collide on the discovery port. Point
clients (N.I.N.A. etc.) at `host:port` directly using the table above.

The sentinel package additionally carries the operator tool
`/usr/bin/rusty-photon-doctor` (diagnosis, `--fix` repair, and the TLS +
credential lifecycle — [docs/services/doctor.md](services/doctor.md))
plus the TLS renewal units `rusty-photon-renew.service` /
`rusty-photon-renew.timer`. There is no separate doctor package:
sentinel is the always-installed supervisor, so its artifact is the
delivery vehicle.

## Building packages

Packages are built natively on the target architecture — in CI on hosted
x86_64 + arm64 runners, nightly (see [Nightly channel](#nightly-channel))
and for the release of each `v*` tag (`.github/workflows/release.yml`),
or on demand directly on the rig / a dev box. From a repo checkout on a
Debian-family machine with Rust installed:

```sh
cargo ci build-packages                    # all services, .deb only
cargo ci build-packages --rpm              # also .rpm
cargo ci build-packages --services qhy-camera,filemonitor
cargo ci build-packages --skip-sdk-staging # offline rebuild from cache
cargo ci build-packages --version 0.1.0+nightly.202607120507.gba09dc9
                                           # nightly version stamp (CI / rollback builds)
```

`--version` takes the workspace version or a `+nightly.` stamp on it,
which the deb carries verbatim, with no `-1` revision. With `--rpm`, the
rpm version is derived from a nightly stamp by rendering `+nightly.` as
rpm's `^` snapshot separator (`0.1.0^202607120507.gba09dc9`) — each
packager renders its own dialect of "sorts after the base release, before
the next one".

The tool installs apt build prerequisites, stages the pinned native
SDKs into `~/.cache/rusty-photon-pkg/` (QHYCCD static lib for the
link; per zwo service its ONE MIT blob, which also becomes that package's
payload; SVBony's library, which svbony-camera links and its package
never ships), then release-builds with the RUNPATH the zwo packages and
svbony-camera load their SDKs through — the two zwo services each in their
own cargo invocation, so feature unification cannot re-union their
per-device SDK links — and runs `cargo deb` (and `cargo generate-rpm` with
`--rpm`) per service. Artifacts land in `dist/<version>/` with a `SHA256SUMS.txt`.

The QHY SDK's version and sha256s are pinned once, in the tools crate's
[`packaging/qhy.rs`](../tools/rusty-photon-ci/src/packaging/qhy.rs), which the
packaging guard
([`tools/rusty-photon-ci/src/pkg_assets.rs`](../tools/rusty-photon-ci/src/pkg_assets.rs))
checks the firmware helper against. The ZWO and SVBony blobs come from the
commits the CI SDK actions pin (`REF` in `install-zwo-sdk/zwo-sdk.mjs` and
`install-svbony-sdk/svbony-sdk.mjs`), which the tool reads, so shipped and
CI-linked SDK bits cannot drift apart.

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
cargo ci build-packages --version "<base>+nightly.<datetime>.g<short-sha>"
```

(`<base>` = the workspace version at that commit. A commit without
`cargo ci build-packages` has `scripts/build-packages.sh --deb-version`
in its place.)

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
`calibrator-flats`, `session-runner`, `polar-align`, `focus-model`) have
no sensible default config, so their units carry `ConditionPathExists=`
on the config file: on a fresh install the unit stays inactive (not
failed) until you write `/etc/rusty-photon/<svc>.json`, then
`systemctl start rusty-photon-<svc>`.

**Tool-provider ordering.** `rusty-photon-rp.service` carries
`After=rusty-photon-calibrator-flats.service rusty-photon-focus-model.service`:
`rp` dials its registered tool providers at startup and fails when one is
down (rp.md § Tool Provider Registration), so a cold boot brings the
shipped providers up first. It is ordering only, not a requirement — an
`rp` config that registers no provider starts either way, and a
config-gated provider unit with no config file is simply skipped.

**Serial-device drivers** (`ppba-driver`, `upbv2-driver`, `gemini-powerbox-adv3-driver`,
`qhy-focuser`, `pa-falcon-rotator`, `pa-scops-oag`, `dsd-fp2`, `star-adventurer-gti`) validate their
hardware eagerly at startup and exit if the device is missing — by design,
so a broken device is never advertised on the network. Until the device is
attached (and its path matches the config), the unit sits in a
restart-every-5s loop; it comes up by itself once the hardware appears.
The cameras and the network-only services serve with no hardware attached.

## First wiring: rusty-photon-doctor

The installer puts bytes on disk; doctor wires the configs.
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
(mtime-triggered in-process reload). Sentinel's watchdog
deliberately ignores the renew unit — it is a scheduled job, not a
daemon to supervise.

## Configuration

Packages ship no config files. Daemons self-create their config on first
start at `/var/lib/rusty-photon/.config/rusty-photon/<svc>.json` (the
shared user's XDG path), reachable via the `/etc/rusty-photon` symlink.
Exceptions: the config-gated six (above) never write one. To change
settings:

```sh
sudo -e /etc/rusty-photon/<svc>.json
sudo systemctl reload rusty-photon-<svc>    # reload-capable services
sudo systemctl restart rusty-photon-<svc>   # the rest
```

Reload-capable (SIGHUP): filemonitor, ppba-driver, upbv2-driver,
gemini-powerbox-adv3-driver, qhy-focuser, sky-survey-camera,
pa-falcon-rotator, pa-scops-oag, dsd-fp2, star-adventurer-gti, qhy-camera,
zwo-camera, svbony-camera, zwo-focuser, planetarium-bridge. Note that a service's setup page (`/setup`) rewrites
its file when a page is saved — hand-edits and page edits share the same
file.

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

## Start the services with every device powered

Start, restart or reload the services, whether at install, upgrade or
reboot, by hand, or by saving a setup page (which reloads its service),
with every device powered, USB and 12 V. A start or a reload looks for each
service's devices then. A camera that is off at that moment is not served
until the service is restarted or reloaded with the camera powered (a
listed camera is a placeholder meanwhile). A serial device that is off
fails its service's start, or ends its process on a reload, and the service
restarts every 5 s until the device answers.

## Camera specifics

**qhy-camera** — QHYCCD's SDK is proprietary and never redistributed.
After installing the package, run once, as root, with internet access:

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
cleanly. ZWO devices keep firmware in onboard flash.

**svbony-camera** — SVBony's SDK carries no license grant at all
([ADR-018](decisions/018-svbony-sdk-no-license-payload-policy.md)), so
unlike ZWO it is never bundled. After installing the package, run once,
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
   ≈ 1.2° × 0.8° solves with it). Go denser (D20 and up) below ~0.6°,
   and to W08 for very wide fields (> 20°). Below ~0.6° the choice
   matters with ASTAP CLI-2026.10.30 and later: their more sensitive
   star detection does not solve CI's 0.42° M101 crop with D05, and does
   with D20, which is why CI pins D20. Either install the database
   `.deb` from the same site (confirm where it lands with `dpkg -L`)
   or unzip the database zip
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

## Removing

```sh
sudo apt-get remove rusty-photon-<svc>   # keeps the service's config + state
sudo apt-get purge rusty-photon-<svc>    # also deletes its config, the state records beside it, and its state dir
```

The shared user, `/var/lib/rusty-photon`, and the `/etc/rusty-photon`
symlink are never removed (shared across packages, Debian convention for
system users). rpm has no purge lifecycle: erase behaves like `remove`;
to fully clean up after an erase, delete
`/var/lib/rusty-photon/.config/rusty-photon/<svc>.json`, the state
records beside it (`<svc>.*.json`) and `/var/lib/rusty-photon/<svc>/` by
hand.

## Verifying a build

```sh
cargo ci verify-packages              # all debs in dist/<version>/
cargo ci verify-packages --rpm        # the rpms, in a Fedora container
cargo ci verify-packages --services filemonitor,zwo-camera --keep
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
services verify enabled-but-inactive-and-not-failed instead. The serial
drivers are held to handshake-attempted plus config-self-created, since
with no device attached they exit and restart rather than serve; the port
probe applies to them only once a driver settles into a steady `active`
(the check waits that out, because a `Type=simple` unit already reads
`active` while the doomed process is still retrying its open). Each zwo
service additionally proves via `ldd` that its binary resolves exactly its
own bundled blob through the RUNPATH and links none of the other ZWO SDKs;
svbony-camera's run first checks that the package does not ship its SDK,
installs it with the package's helper as an operator would, and proves
the same RUNPATH resolution. Rootless podman cannot apply the units'
sandboxing, so the tool resets the hardening inside the container — hardening is
verified on real hosts with `systemd-analyze security
rusty-photon-<svc>.service`. An interrupted run leaves its container,
`rusty-photon-verify-<pid>`, running; `podman rm -f` it.

CI runs both flavors on both architectures before anything publishes:
the nightly's `linux` legs and `release.yml`'s `build-linux` legs build
with `cargo ci build-packages --rpm` and gate the upload on
`cargo ci verify-packages` and `cargo ci verify-packages --rpm`.

Expected `lintian` findings (accepted, not bugs):
`custom-library-search-path` on every package (the RUNPATH is injected
uniformly; only the zwo packages and svbony-camera use it); `no-changelog` /
`no-manual-page` / `copyright-without-copyright-notice` pre-1.0; `unstripped-binary-or-object`
and `hardening-no-relro` on the vendored ZWO blobs (shipped exactly as
published); `embedded-library` on qhy-camera's statically linked SDK;
`appstream-metadata-missing-modalias-provide` on the camera packages' udev
rules; `empty-field Depends` and `unstripped-binary` on our own binaries
only on ad-hoc builds from non-Debian hosts.

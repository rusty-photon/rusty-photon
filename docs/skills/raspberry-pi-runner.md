# Skill: Raspberry Pi 5 Self-Hosted Nightly Runner

## When to Read This

- Setting up the Raspberry Pi 5 self-hosted runner for the first time
- Re-registering the runner after a token expiry or factory reset
- Debugging a red `pi-nightly` workflow run
- Decommissioning the runner (removing from GitHub + the Pi itself)
- Auditing the security posture of self-hosted runners on this repo

## Prerequisites

- A Raspberry Pi 5 (Linux/ARM64) running Ubuntu 24.04 LTS or newer
- SSH access to the Pi as a sudo-capable user
- Owner or admin access to the `rusty-photon/rusty-photon` GitHub repo
- A network position that lets the Pi reach `github.com` and
  `*.actions.githubusercontent.com` over HTTPS. The nightly needs **nothing on
  RFC1918** — every input (GitHub, crates.io, the vendor SDK downloads,
  OmniSim/Pebble releases) is on the WAN — so the Pi belongs on the same
  fenced runner VLAN as the Proxmox pool (see §"Network position" under
  Operational Notes), not on the operator's general LAN

## Why a Self-Hosted Runner (and Why It Is Safe Here)

GitHub-hosted runners only cover x86_64 (`ubuntu-latest`, `macos-latest`,
`windows-latest`). The Pi 5 is ARM64, so a Pi nightly catches arch-specific
regressions the rest of CI cannot: atomics, alignment, vendored C
dependencies such as `cfitsio` / `fitsio-sys`, cross-arch feature
unification breaks, and BDD scenarios that exercise endian-sensitive
serialisation paths.

This repo is **public**, which is the case GitHub's own docs warn against:
"We recommend that you only use self-hosted runners with private
repositories. This is because forks of your public repository can
potentially run dangerous code on your self-hosted runner machine by
creating a pull request that executes the code in a workflow."

The threat is concrete: a malicious PR can edit the workflow YAML, and on
`pull_request` events GitHub Actions runs the **PR's version** of that
YAML, not main's. So if any workflow on a self-hosted runner triggers on
`pull_request`, a fork can execute arbitrary commands on the Pi during PR
validation.

`pi-nightly.yml` neutralises this by triggering **only** on `schedule` and
`workflow_dispatch`. Scheduled runs always use the workflow file from the
default branch (main), and only the repo owner can push to main, so PRs
cannot influence what executes on the Pi. The job adds two more belts:
`ref: main` on `actions/checkout` and `if: github.ref == 'refs/heads/main'`
at the job level.

### Why no `pull_request` trigger

The "I'd like ARM coverage on PRs too" temptation must be resisted on a
public repo until either (a) the runner is moved to a private mirror, or
(b) a Just-In-Time (JIT) ephemeral runner pool with PR-approval gating is
set up. Option (b) now exists for x86_64 Linux — see
docs/skills/proxmox-runner-pool.md — and it DOES serve same-repo PR jobs,
but only under the six-layer contract of
[ADR-020](../decisions/020-ephemeral-self-hosted-runners-for-pr-checks.md)
(fork-excluding routing, JIT single-use VMs, no credentials on the runner,
VLAN fencing, kill switch). Of those layers only the VLAN fence applies to
THIS Pi runner (it lives on the same runner VLAN as the pool — see
§"Network position"); it remains persistent and credentialed. For this file
the rule stays binary: `schedule:` and `workflow_dispatch:` only.

If you ever need ARM-on-PR coverage, prefer GitHub's free `ubuntu-24.04-arm`
runner (free for public repos) — see
[github.com/actions/runner-images](https://github.com/actions/runner-images).

## One-Time Setup

The setup script `scripts/setup-pi-runner.sh` is the canonical, idempotent
path. The sections below explain what the script does so an operator can
audit or reproduce it manually.

### 1. System dependencies

The Pi needs a small set of packages that GitHub-hosted runners pre-install
but Ubuntu Server does not:

```bash
sudo apt-get update
sudo apt-get install -y \
  build-essential \
  pkg-config \
  curl \
  git \
  jq \
  libssl-dev \
  libcfitsio-dev \
  libusb-1.0-0 \
  unzip \
  ca-certificates
```

`libcfitsio-dev` is required by the `fitsio-sys` build script. Without it
the `rp-fits`, `filemonitor`, and `sky-survey-camera` packages fail to
compile (this is the "use `-p <package>`" caveat that the user-level
`MEMORY.md` references). `libssl-dev` is required by transitive C-FFI
crates in the workspace. The **libusb-1.0 runtime** (`libusb-1.0-0`) is the
shared symlink target for the QHYCCD, ZWO, **and SVBony** sudo-free link
paths (next three subsections); the `-dev` package is deliberately **not**
installed — the unversioned `libusb-1.0.so` linker name is provided per-run
instead.

#### QHYCCD SDK (for `qhy-camera`)

`qhy-camera` links the proprietary QHYCCD SDK (`libqhyccd-sys` →
`static=qhyccd` + `libusb-1.0` + `stdc++`). The Pi5 arm64 nightly builds the
full workspace, so the SDK (pinned **26.06.04**, aarch64) must be available at
link time. The runner does **not** pre-provision it any more — `pi-nightly.yml`
provisions it **per run** with the `ivonnyssen/qhyccd-sdk-install@v4` action in
its sudo-free **`install: env`** mode: the action downloads the SDK from
**qhyccd.com** (publicly, no auth), extracts it under the workspace, and exports
`QHYCCD_SDK_DIR` (the directory holding `libqhyccd.a`). `libqhyccd-sys`'s
`build.rs` prefers `QHYCCD_SDK_DIR` on Linux (falling back to `/usr/local/lib`)
and adds **only that one dir** to the link search; `libqhyccd.a` itself is
linked **statically**, so no QHYCCD `.so` runtime chain, no `ldconfig`, and no
`LD_LIBRARY_PATH` are needed and nothing is written into `/usr/local`.

The static `libqhyccd.a` does, however, pull in a *dynamic* `-lusb-1.0`. On a
sudo-less runner there is no `libusb-1.0-0-dev`, hence no unversioned
`libusb-1.0.so` for the linker — so the **Symlink libusb for the QHYCCD static
link** step in `pi-nightly.yml` drops that linker-name symlink into
`QHYCCD_SDK_DIR` (the one dir `build.rs` searches), pointing at the libusb-1.0
*runtime* `.so.0`. Without it the build fails with `cannot find -lusb-1.0`
(this was [issue #402](https://github.com/rusty-photon/rusty-photon/issues/402)).
This mirrors the ZWO sudo-free symlink (next subsection); both share the one
`libusb-1.0` runtime package installed in §1.

This is what keeps the runner **sudo-less** (public-repo safety — the job user
has no root, so a dependency `build.rs` cannot escalate) *and* self-healing: a
new native-SDK service or an SDK version bump no longer requires re-running
`setup-pi-runner.sh` by hand — the workflow re-fetches the SDK every night.
Accordingly, `setup-pi-runner.sh`'s `=== 1b. QHYCCD SDK ===` section no longer
installs anything; it is just a pointer to this per-run flow. The ZWO SDK now
follows the same sudo-free per-run model (next subsection).

The GitHub-hosted **ubuntu, macOS, and Windows** jobs install the SDK via the
same action in its default (system) mode; only the sanitizer job (`safety.yml`)
and the per-PR sim-only legs exclude it (`QHYCCD_SKIP_NATIVE_LINK=1`). The Pi
covers linux-arm64.

#### ZWO ASI/EFW SDK (for `zwo-camera`)

`zwo-camera` links the MIT-licensed ZWO ASI/EFW SDK unconditionally via
`zwo-rs → libzwo-sys`, whose `build.rs` emits `-lASICamera2 -lEFWFilter
-lstdc++ -lusb-1.0 -ludev` on Linux **even under `--features simulation`** (the
link is env-gated by `ZWO_SKIP_NATIVE_LINK`, not feature-gated). The full
workspace build therefore needs the SDK at link time. Like QHYCCD, the runner
no longer pre-provisions it — `pi-nightly.yml` runs the local
`./.github/actions/install-zwo-sdk` action in its **sudo-free** mode
(`sudo: "false"`), which:

- downloads the INDI-vendored ZWO blobs (`libASICamera2`/`libEFWFilter`, plus
  best-effort `libEAFFocuser`) for `armv8` into `$RUNNER_TEMP/zwo-sdk/lib`,
  pinned by the action's `ref` (bump it to adopt a newer SDK — no manual
  re-provision);
- satisfies the unversioned `-lusb-1.0`/`-ludev` link names **without any -dev
  package** by symlinking them to the system *runtime* libs
  (`libusb-1.0.so.0`, `libudev.so.1`) inside that same dir — `build.rs` puts
  `ZWO_SDK_LIB_DIR`'s `-L` ahead of `/usr/local/lib`, and a single `-L`
  resolves every `-l` (then `--as-needed` drops the unused ones);
- exports `ZWO_SDK_LIB_DIR` (link search) and `LD_LIBRARY_PATH` (the blobs ship
  **no SONAME**, so the test binaries' `NEEDED` is the bare `libASICamera2.so`,
  which must be on the loader path for the nextest/BDD/doctest steps).

The two prerequisites the sudo-free step cannot install itself are stable host
packages installed once by §1 of `setup-pi-runner.sh`: **clang + libclang-dev**
(bindgen) and the **libusb-1.0 runtime** (`libusb-1.0-0`; it is the
`libusb-1.0.so` symlink target for the ZWO, QHYCCD, and SVBony link paths, and
the blob's own runtime dependency).
`libudev.so.1` ships with systemd. If the step ever errors with `… not found`,
install the named package once and re-run — see Troubleshooting. This keeps the
runner sudo-less *and* self-healing for ZWO exactly as for QHYCCD; the
GitHub-hosted x86 jobs keep using the action in its default sudo/system mode.

#### SVBony camera SDK (for `svbony-camera`)

`svbony-camera` links the SVBony camera SDK unconditionally via
`svbony-rs → libsvbony-sys`, whose `build.rs` emits `-lSVBCameraSDK
-lusb-1.0` on Linux **even under `--features simulation`** (the link is
env-gated by `SVBONY_SKIP_NATIVE_LINK`, not feature-gated — see
docs/services/svbony-camera.md "Native dependency & build gating"). The full
workspace build therefore needs the SDK at link time, exactly like ZWO. The
runner does not pre-provision it either — `pi-nightly.yml` runs the local
`./.github/actions/install-svbony-sdk` action in its **sudo-free** mode
(`sudo: "false"`), which:

- downloads the INDI-vendored SVBony blob (`libSVBCameraSDK`) for `armv8`
  into `$RUNNER_TEMP/svbony-sdk/lib`, pinned by the action's `ref` (bump it
  to adopt a newer SDK — no manual re-provision);
- satisfies the unversioned `-lusb-1.0` link name **without any -dev
  package** by symlinking it to the system *runtime* lib
  (`libusb-1.0.so.0`) inside that same dir, exactly as the ZWO/QHYCCD steps
  do — `build.rs` puts `SVBONY_SDK_LIB_DIR`'s `-L` ahead of `/usr/local/lib`;
- exports `SVBONY_SDK_LIB_DIR` (link search) and `LD_LIBRARY_PATH` (the blob
  installs no SONAME on the sudo-free path — no `ldconfig` is run — so the
  nextest/BDD/doctest binaries need it on the loader path directly).

The only prerequisite the sudo-free step cannot install itself is the same
**libusb-1.0 runtime** (`libusb-1.0-0`) the QHYCCD/ZWO steps already share,
installed once by §1 of `setup-pi-runner.sh`. No clang/libclang is needed —
`libsvbony-sys`'s FFI is hand-written, not bindgen'd. This keeps the runner
sudo-less *and* self-healing for SVBony exactly as for ZWO/QHYCCD; the
GitHub-hosted x86 jobs keep using the action in its default sudo/system mode.

### 2. Dedicated unprivileged user

The runner must not run as root. Create a dedicated user with no sudo
rights, no shell login (only the runner's `actions-runner` directory
matters), and a fresh home dir:

```bash
sudo useradd -m -s /usr/sbin/nologin -U gh-runner
```

The `nologin` shell prevents interactive logins; the runner's `run.sh` is
invoked by systemd, which doesn't need a login shell.

If the runner needs `rustup` (which it does — `dtolnay/rust-toolchain@stable`
calls it), the toolchain lives under `~gh-runner/.rustup` and
`~gh-runner/.cargo`. That's fine — both are inside the dedicated user's
home and isolated from any other user on the Pi.

### 3. Rustup + stable toolchain

`dtolnay/rust-toolchain@stable` installs rustup on first call if missing,
but it's faster to pre-install:

```bash
sudo -u gh-runner bash -c '
  curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
  echo "source $HOME/.cargo/env" >> $HOME/.bashrc
'
```

`cargo-nextest` is installed by the workflow via `taiki-e/install-action`,
so no pre-install needed.

### 4. Download and register the runner

GitHub's runner ships as a single tarball per OS/arch combo. Get the
latest ARM64 Linux runner from
[github.com/actions/runner/releases](https://github.com/actions/runner/releases).

```bash
sudo -u gh-runner bash -c '
  mkdir -p $HOME/actions-runner
  cd $HOME/actions-runner
  RUNNER_VERSION=2.334.0   # check releases page for current
  curl -fsSL -o actions-runner.tar.gz \
    https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-arm64-${RUNNER_VERSION}.tar.gz
  tar xzf actions-runner.tar.gz
  rm actions-runner.tar.gz
'
```

Registration requires a **runner registration token**, which is
short-lived (expires in ~1 hour) and must be fetched from GitHub:

> Repo → Settings → Actions → Runners → New self-hosted runner → copy the
> token shown in the `./config.sh ...` snippet

Then on the Pi:

```bash
sudo -u gh-runner bash -c '
  cd $HOME/actions-runner
  ./config.sh \
    --url https://github.com/rusty-photon/rusty-photon \
    --token <TOKEN_FROM_GITHUB_UI> \
    --name pi5-nightly \
    --labels raspberry-pi \
    --work _work \
    --unattended \
    --replace
'
```

The `--labels raspberry-pi` value must match the workflow's
`runs-on: [self-hosted, Linux, ARM64, raspberry-pi]` (the first three
labels are auto-applied by GitHub based on the runner's environment).

`--replace` lets re-registration overwrite a stale entry without manual
deregistration in the UI — useful if the Pi is reimaged.

### 5. Install as a systemd service

GitHub ships an installer for this. Note the `sudo bash -c '...'` wrapping:
`svc.sh` writes to `/etc/systemd/system/` (root-only) and reads template
files from its own directory, but Ubuntu Server 24.04 creates
`/home/gh-runner` with mode `0750` so your regular sudo user can't `cd`
into it. Running the whole compound under `sudo` lets root do both the
directory entry and the install:

```bash
sudo bash -c 'cd /home/gh-runner/actions-runner && ./svc.sh install gh-runner && ./svc.sh start'
```

`svc.sh` derives the unit name from the URL `config.sh` was given plus the
runner name — `actions.runner.<owner>-<repo>.<runner>.service` — and
freezes it at `svc.sh install` time. A fresh install against the org URL
yields `actions.runner.rusty-photon-rusty-photon.pi5-nightly.service`; an
installation configured before the org transfer keeps
`actions.runner.ivonnyssen-rusty-photon.pi5-nightly.service` (the
currently deployed Pi is one) until `svc.sh uninstall` + `install` is
re-run. List what is actually installed with
`systemctl list-units 'actions.runner.*'`. Verify (substituting your
installed unit name):

```bash
systemctl status actions.runner.rusty-photon-rusty-photon.pi5-nightly.service
sudo journalctl -u actions.runner.rusty-photon-rusty-photon.pi5-nightly.service -f
```

From GitHub: Repo → Settings → Actions → Runners — the runner should show
as **Idle** within a few seconds.

## Operational Notes

### Network position

The Pi lives on the **runner VLAN** — the fenced VLAN the Proxmox pool clones
use (docs/skills/proxmox-runner-pool.md "Security Model"): off-VLAN the router
allows exactly the WAN and DNS, so a compromised nightly job can reach neither
the observatory hosts nor the operator's machines. `pi-nightly` needs nothing
else — it never touches the LAN build cache (it is a Cargo job) or any share.
The Pi takes its address by DHCP (`dhcp4: true`, no static netplan) with a
fixed-IP reservation on that VLAN so the address survives reboots; the
address, MAC and VLAN number are inventory data and stay out of this public
repo. Operator SSH comes in from the admin network through a router allow
rule (that direction is not fenced); if the admin machine's zone is not
covered, `ssh -J <a host that is>` is the workaround.

Two consequences of sharing the VLAN with the pool:

- The ephemeral clones can reach the Pi at L2 (their own firewall drops
  *inbound* only). The Pi should publish nothing to that VLAN: keep sshd
  reachable from the admin network only, e.g. `ufw allow from <admin-cidr>
  to any port 22 proto tcp`, `ufw deny from <runner-vlan-cidr> to any port 22
  proto tcp`, `ufw enable` — the same binding discipline the pool doc
  requires of the LAN cache host.
- Runner registration is IP-agnostic (an outbound long-poll), so moving the
  Pi between VLANs needs no re-registration: change the switch port's
  **Native VLAN / Network** in UniFi Port Manager (Devices → switch → Ports →
  the port; the *client* entry has no VLAN field), then **bounce the link** —
  a native-VLAN change does not drop link, so the Pi otherwise keeps its old
  lease and goes dark until renewal — and then **restart the runner unit**:
  the listener does not survive an address change underneath it (an
  in-flight HTTPS request on the vanished address hangs without a timeout,
  and the runner sits *Offline* indefinitely). Verify in this order:
  `ip -4 -br addr` shows a lease on the runner VLAN (the runner showing
  *Idle* proves DNS + WAN, **not** which VLAN — a first bounce can land on
  the port's previous network); the runner is *Idle*; then a
  `workflow_dispatch` run to prove the SDK downloads and the full build (a
  dispatch failure does not open the tracking issue).

### Triggering a run manually

`pi-nightly.yml` exposes `workflow_dispatch`. From the Actions tab:

> Actions → pi-nightly → Run workflow → Branch: main → Run workflow

`workflow_dispatch` enforces the same `if: github.ref == 'refs/heads/main'`
gate as the scheduled trigger, so this can only run against main.

### Reading logs

Live job logs appear in the Actions tab as usual. The runner-side daemon
log (start/stop, job pickup, deregistration events) is in journald:

```bash
sudo journalctl -u actions.runner.rusty-photon-rusty-photon.pi5-nightly.service -f
```

(Substitute your installed unit name — see §5's naming note.)

The job's working tree lives under `~gh-runner/actions-runner/_work/rusty-photon/rusty-photon`
between runs. The runner's default behaviour is to wipe the workspace
between jobs but preserve cached toolchains (`.rustup`, `.cargo`) in the
user's home dir.

### Disk usage

Cargo's `target/` directory under `_work/` can grow to 5-10 GB across a
mixed workspace + all-features build. The Swatinem cache only restores
incremental state, so the active `target/` is unavoidable. Plan for at
least 32 GB of free disk on the Pi (an external SSD is strongly
recommended over an SD card for write durability).

### Notifications

`pi-nightly.yml` includes a `notify-on-failure` job that opens or updates
a `pi-nightly`-labelled issue in the repo on every scheduled failure.
This runs on a GitHub-hosted runner, so it still fires when the Pi itself
is offline (the `arm64-stable` job will fail with "runner offline" and
`notify-on-failure` reports it).

GitHub also sends an email by default to the workflow author when a
scheduled run fails — controlled at
github.com → Settings → Notifications → Actions.

## Re-Registering the Runner

The runner registration token GitHub gave you at setup time was one-time;
it cannot be re-used. To re-register (typically after a reimage or moving
the Pi):

1. On GitHub: Repo → Settings → Actions → Runners → click the runner row →
   "Remove runner" (or do nothing — `--replace` at config time handles a
   stale entry).
2. Generate a fresh token from the "New self-hosted runner" UI on that same
   page.
3. On the Pi (the `sudo -u gh-runner bash -c '...'` wrapping is for the
   same `0750` home-directory reason as §5 — `config.sh` expects to run
   from inside its own directory). Use your installed unit name in the
   stop/start lines (`systemctl list-units 'actions.runner.*'`; the
   deployed Pi predates the org transfer, so today that is the
   `ivonnyssen-` form). Re-registering via `config.sh` does **not**
   rename the unit — the install-time name persists and keeps working;
   only `svc.sh uninstall` + `install` re-derives it:
   ```bash
   sudo systemctl stop actions.runner.rusty-photon-rusty-photon.pi5-nightly.service
   sudo -u gh-runner bash -c 'cd /home/gh-runner/actions-runner && ./config.sh remove --token <REMOVAL_TOKEN>'
   sudo -u gh-runner bash -c 'cd /home/gh-runner/actions-runner && ./config.sh \
     --url https://github.com/rusty-photon/rusty-photon \
     --token <FRESH_REGISTRATION_TOKEN> \
     --name pi5-nightly \
     --labels raspberry-pi \
     --work _work \
     --unattended \
     --replace'
   sudo systemctl start actions.runner.rusty-photon-rusty-photon.pi5-nightly.service
   ```

The removal token and registration token are different and are both shown
in the GitHub UI when needed.

## Decommissioning

If the Pi is going away or the workflow is being retired:

1. On the Pi:
   ```bash
   sudo bash -c 'cd /home/gh-runner/actions-runner && ./svc.sh stop && ./svc.sh uninstall'
   sudo -u gh-runner bash -c 'cd /home/gh-runner/actions-runner && ./config.sh remove --token <REMOVAL_TOKEN>'
   sudo userdel -r gh-runner
   ```
2. On GitHub: confirm the runner is gone from
   Settings → Actions → Runners.
3. Delete `.github/workflows/pi-nightly.yml`, this runbook, and
   `scripts/setup-pi-runner.sh`. Update `README.md` and `.github/DOCS.md`
   to drop the references.

## Troubleshooting

### Runner shows as "Offline" in the GitHub UI

Most common cause: systemd service stopped or network outage. Check:

```bash
systemctl status actions.runner.ivonnyssen-rusty-photon.pi5-nightly.service
sudo journalctl -u actions.runner.ivonnyssen-rusty-photon.pi5-nightly.service -n 200
ping -c 3 github.com
```

If the service is `failed`, look for "Token has expired" — that means the
registration was revoked from the UI side. Re-register (see above).

If the service is `active` and the Pi has a good address, gateway and DNS,
but GitHub still shows Offline **after the Pi's IP address changed** (VLAN
move, new DHCP lease, link bounce): the listener is hung on an HTTPS request
it issued from the old address — there is no timeout, so it stays Offline
indefinitely and the unit's journal shows nothing. `sudo systemctl restart
'actions.runner.*'` clears it (`_diag/Runner_*.log` shows the last request
before the silence). Note that from a network zone the router does not
route to the runner VLAN, `ping`/`ssh` failing proves nothing — the runner's
status in the GitHub UI is the liveness signal (it needs only WAN + DNS),
and `ip -4 -br addr` on the Pi is the only proof of *which* network it is on.

### `cargo build` fails with "could not find pkg-config" or "openssl-sys"

System deps were not installed for the `gh-runner` user's PATH. Re-run the
apt-get block from §"System dependencies". Confirm `pkg-config --version`
works as the `gh-runner` user:

```bash
sudo -u gh-runner pkg-config --version
```

### `fitsio-sys` build script fails

`libcfitsio-dev` missing. Install it; no rebuild flags needed.

### `cargo build` fails with `could not find native static library 'qhyccd'`

`libqhyccd-sys` could not find `libqhyccd.a` on the linker search path. On the
Pi nightly the SDK is provisioned per-run by the **Install QHYCCD SDK
(sudo-free)** step (`ivonnyssen/qhyccd-sdk-install@v4`, `install: env`), which
exports `QHYCCD_SDK_DIR`. Check, in order:

- That step ran and printed `QHYCCD_SDK_DIR=…/usr/local/lib` before
  `cargo build`. If it failed to download, the qhyccd.com URL or pinned
  `version:` may have moved — confirm
  `https://www.qhyccd.com/file/repository/publish/SDK/260604/sdk_linux_arm64_26.06.04.tar.gz`
  still resolves.
- The `@v4` (or later) tag of `ivonnyssen/qhyccd-sdk-install` exists and supports
  `install: env`. Older tags (`@v3` and earlier) only do the sudo system install
  and will not export `QHYCCD_SDK_DIR`.
- `QHYCCD_SKIP_NATIVE_LINK` is **not** set on this job (it must exercise the real
  ARM64 static link; the skip flag is only for the sim-only x86 legs).

A manual `/usr/local/lib` install (the old `setup-pi-runner.sh §1b` behaviour)
still satisfies the link via `build.rs`'s fallback, but is no longer required or
performed by setup.

### `cargo build` fails with `cannot find -lusb-1.0` (compiling `libqhyccd-sys`)

The static `libqhyccd.a` pulls in a dynamic `-lusb-1.0`, but the sudo-less
runner has no `libusb-1.0-0-dev` (no unversioned `libusb-1.0.so` linker name).
This was [issue #402](https://github.com/rusty-photon/rusty-photon/issues/402).
The **Symlink libusb for the QHYCCD static link (sudo-free)** step in
`pi-nightly.yml` fixes it by linking `libusb-1.0.so` → the runtime `.so.0`
inside `QHYCCD_SDK_DIR`. Check, in order:

- That step ran and printed `linked …/libusb-1.0.so -> …/libusb-1.0.so.0`
  before `cargo build` (it runs right after the QHYCCD SDK step).
- The step did **not** abort with `libusb-1.0 runtime … not found`. That means
  the host lacks the libusb-1.0 runtime — install it once with sudo and re-run:
  `sudo apt-get install -y libusb-1.0-0` (it is in §1 of `setup-pi-runner.sh`,
  so re-running the setup script is the catch-all fix).

### `cargo build` fails with `cannot find -lASICamera2` / `-lEFWFilter` / `-lusb-1.0` / `-ludev`

`libzwo-sys` could not find the ZWO SDK (or the libusb/libudev link names) on
the search path while linking `zwo-camera`. On the Pi nightly these are
provided per-run by the **Install ZWO SDK (sudo-free)** step
(`./.github/actions/install-zwo-sdk`, `sudo: "false"`). Check, in order:

- That step ran and printed `exported ZWO_SDK_LIB_DIR + LD_LIBRARY_PATH=…` before
  `cargo build`. If a blob download failed, confirm the action's pinned `ref`
  still resolves under `https://github.com/indilib/indi-3rdparty/raw/<ref>/libasi/armv8/`.
- The step did **not** abort with `… not found`. That message means the host is
  missing a runtime prerequisite the sudo-free path symlinks against — install it
  once with sudo and re-run: `sudo apt-get install -y libusb-1.0-0` (provides
  `libusb-1.0.so.0`) — `libudev.so.1` ships with systemd. `clang`/`libclang-dev`
  (bindgen) must also be present; all three are in §1 of `setup-pi-runner.sh`, so
  re-running it is the catch-all fix.
- `ZWO_SKIP_NATIVE_LINK` is **not** set on this job (the Pi must exercise the real
  ARM64 link; the skip flag is only for the sim-only x86 legs in
  `test.yml`/`safety.yml`/`publish-readiness.yml`).

### `cargo build` fails with `cannot find -lSVBCameraSDK` / `-lusb-1.0` (compiling `libsvbony-sys`)

`libsvbony-sys` could not find the SVBony SDK (or the libusb-1.0 link name)
on the search path while linking `svbony-camera`. On the Pi nightly these
are provided per-run by the **Install SVBony SDK (sudo-free)** step
(`./.github/actions/install-svbony-sdk`, `sudo: "false"`). Check, in order:

- That step ran and printed `exported SVBONY_SDK_LIB_DIR + LD_LIBRARY_PATH=…`
  before `cargo build`. If a blob download failed, confirm the action's
  pinned `ref` still resolves under
  `https://github.com/indilib/indi-3rdparty/raw/<ref>/libsvbony/`.
- The step did **not** abort with `… not found`. That message means the host
  is missing the runtime prerequisite the sudo-free path symlinks against —
  install it once with sudo and re-run: `sudo apt-get install -y
  libusb-1.0-0` (provides `libusb-1.0.so.0`; it is in §1 of
  `setup-pi-runner.sh`, so re-running it is the catch-all fix).
- `SVBONY_SKIP_NATIVE_LINK` is **not** set on this job (the Pi must exercise
  the real ARM64 link; the skip flag is only for the sim-only x86 legs and
  Bazel — see docs/services/svbony-camera.md "Native dependency & build
  gating").
- If this step is simply **missing** from the job (the original cause of
  [issue #669](https://github.com/rusty-photon/rusty-photon/issues/669)): the
  full-workspace build links `svbony-camera` unconditionally, so this step
  must run before `cargo build --workspace`, in the same place the ZWO SDK
  step does.

### nextest runs but BDD hangs / OmniSim crashes

The Pi may be hitting one of the same intermittent OmniSim issues
addressed in `test.yml`. The workflow uploads OmniSim logs on failure to
the `omnisim-logs-pi-nightly` artifact — download it from the failed run
and investigate per `crates/bdd-infra/src/rp_harness/omnisim.rs`.

### Cache misses every night

Confirm the `shared-key` in `pi-nightly.yml` matches across runs
(`linux-arm64-stable`). GitHub's Actions cache has a 10 GB cap per repo,
so a very full cache namespace can also evict ARM entries — check repo
Settings → Actions → Caches.

## References

- [CLAUDE.md / AGENTS.md](../../CLAUDE.md) — operating rules (rules 4–6 govern
  pre-push gates and commit format)
- [docs/skills/pre-push.md](pre-push.md) — quality-gate suite this nightly
  approximates
- [.github/workflows/pi-nightly.yml](../../.github/workflows/pi-nightly.yml) —
  the workflow itself (read the header comment for the security model)
- [scripts/setup-pi-runner.sh](../../scripts/setup-pi-runner.sh) — idempotent
  setup script
- [GitHub: Self-hosted runners — security](https://docs.github.com/en/actions/hosting-your-own-runners/managing-self-hosted-runners/about-self-hosted-runners#self-hosted-runner-security)
  — the upstream warning this skill mitigates
- [GitHub Actions runner releases](https://github.com/actions/runner/releases)
  — current `linux-arm64` tarballs and changelogs

# Nightly Releases Plan — a rolling nightly channel for every OS target

## Goal

Publish a nightly release built from the latest `main`: one rolling GitHub
prerelease carrying installable packages for Debian (`.deb`, x86_64 +
arm64), Fedora (`.rpm`, x86_64 + arm64), Windows (suite MSI, x64), and
macOS (Homebrew tap, arm64) — each OS phase independently implementable.
Today packaging is on-demand only (`scripts/build-packages.sh` on a target
machine; `release.yml` on a `v*` tag); the nightly channel gives the rigs
and any test box an always-current, verified upgrade path between releases.

N5 adds a second consumption path for the Linux legs: a real `apt`/`dnf`
repository (Cloudflare R2-hosted) so a Debian/Fedora machine picks up
nightlies via `apt upgrade`/`dnf upgrade` instead of a manual download —
the GitHub-release assets stay as the `SHA256SUMS.txt`-indexed manual
path, unchanged.

This plan changes nothing in
[ADR-012](../decisions/012-service-packaging-architecture.md)/
[ADR-013](../decisions/013-native-sdk-payload-policy.md)/
[ADR-014](../decisions/014-zwo-per-device-services-and-link-features.md)/
[ADR-015](../decisions/015-windows-packaging-architecture.md) — it adds a
release *channel* on top of the packaging those ADRs define. `release.yml`
stays tag-triggered and untouched here; the scripts this plan builds are
deliberately reusable so the deferred `release.yml` generalization
(PR-7 in [service-packaging.md](service-packaging.md)) becomes thin.

## Implementation Status

| Phase | Description | Status | Branch / PR |
|-------|-------------|--------|-------------|
| N0 | Tech spike: hosted-arm64 verify, timings, asset naming, version dialects — settles the Orange Pi question | **Done** (2026-07-13; findings below — Orange Pi: no-go) | scratch branch `spike/n0-nightly-packaging` (deleted) |
| N1 | Debian anchor: `nightly-packages.yml` shared spine + `.deb` legs (x86_64 + arm64), rolling release, docs | **Done** (2026-07-13; first publish = first post-merge run) | PR #508 |
| N2 | Fedora: `.rpm` build on both arches + Fedora lifecycle verify leg | **Done** (2026-07-13; rpms first published by that day's scheduled run) | PR #513 |
| N3 | Windows: suite-MSI leg (strictly after W5 of [windows-packaging.md](windows-packaging.md)) | **Done** (2026-07-13; first MSI publish = next scheduled run, whose msi job skips the upgrade seed gracefully — the run after proves MSI-over-MSI). The upgrade seed was **suspended 2026-07-18** pre-1.0 ([#582](https://github.com/rusty-photon/rusty-photon/issues/582): re-enable with doctor `--fix` in the loop once D7 ships doctor in the packages) | PR #509 |
| N4 | macOS: per-service arm64 tarballs + Homebrew tap channel + `verify-brew.sh` | **Done** (2026-07-13; first macOS publish = next scheduled run) | PR #519 |
| N5 | Debian/Fedora package repositories: Cloudflare R2-hosted `apt`/`dnf` channels for the N1/N2 `.deb`/`.rpm` legs | **Done** (2026-07-16: #535 merged and, after PR #547's S3-API auth fix, the first publish landed the same day) | PRs #535, #547 |

N1 is the anchor (it builds the shared spine); N2, N3, N4 are mutually
independent afterwards. N3 is gated only on W5; N4 has synergy with PR-7
(stable-channel formula generation) and the two are best done as one arc.
N5 depends only on N1+N2 (it repackages their already-verified `.deb`/
`.rpm` output) and is otherwise independent of N3/N4.

## Decisions (fixed)

- **Channel shape: one rolling `nightly` prerelease.** A single tag,
  force-moved to the packaged commit each night; all assets replaced; no
  dated history. The release body records source SHA + date + asset table.
  Stable asset URLs make a future rig-update helper trivial. Rolling back
  means rebuilding a known-good SHA on demand. The tag MUST NOT match
  `v*` — that pattern triggers `release.yml`.
- **Source: HEAD of `main`, skip-if-unchanged.** Main is already PR-gated
  by the Bazel checks; no last-green lookup. If HEAD equals the SHA the
  `nightly` tag points at, the run exits early — no rebuild, no asset
  churn.
- **All-or-nothing publish.** Every *enabled* OS leg must pass its build +
  lifecycle verification before anything is published, so the release is
  always one coherent SHA with a complete asset set. A flaky leg means "no
  nightly today" plus a tracking issue — never a mixed-SHA release.
- **Version dialects.** One version job computes the base version (from
  the workspace `Cargo.toml`) + UTC date-time to the minute + short SHA,
  and each packager renders its own dialect of "sorts after 0.1.0, before
  0.1.1, upgrades in place". The stamp was date-only until 2026-07-18
  ([#584](https://github.com/rusty-photon/rusty-photon/issues/584)): the
  `g<sha>` suffix compares as hex, not as history, so a second publish on
  the same UTC day could sort *below* the first and apt/dnf then refuse
  the upgrade — the minute component makes the timestamp carry all the
  ordering. Date-only examples elsewhere in this plan are historical
  records of the pre-change shape and are left as written:

  | Target | Nightly version | Mechanics |
  |--------|-----------------|-----------|
  | Debian `.deb` | `0.1.0+nightly.202607120507.gabc1234` | dpkg sorts `+…` above `0.1.0`, below `0.1.1`; `apt` upgrades over an on-demand `0.1.0-1` install |
  | Fedora `.rpm` | `0.1.0^202607120507.gabc1234` | rpm forbids `+` in Version; `^` is rpm's post-release snapshot operator with the same ordering |
  | Windows MSI | ProductVersion `0.1.0.<YYDDD>` | Windows Installer compares only the first three ProductVersion fields; a 4th field is legal to author and **display-only** — it shows as the version in ARP, which is why we keep it (a plain `0.1.0` would render the stable release and every nightly identically there). Upgrade logic thus sees every nightly as `0.1.0`, hence `MajorUpgrade AllowSameVersionUpgrades="yes"`; the full string travels in the filename and ARP comments. `YYDDD` = 2-digit year × 1000 + day-of-year (within the 65535 per-field authoring cap through 2065). The date deliberately does NOT ride in the *compared* third field — `0.1.<YYDDD>` would sort above every future release |
  | macOS | full string in the tarball filename + tap formula `version` | no installer database; Homebrew compares formula versions (see flagged unknowns for the `+` tokenization check) |

  Known caveat (Debian): once a machine runs nightlies, a plain on-demand
  `0.1.0-1` build is a *downgrade* for apt (`--allow-downgrades` or
  `dpkg -i`). Accepted.
- **Build hosts: GitHub-hosted runners.** `ubuntu-latest` (x86_64) +
  `ubuntu-24.04-arm` (arm64; free on public repos, already used by
  `install-astap.yml`), `windows-latest`, `macos-latest` (Apple Silicon).
  No self-hosted machines in the nightly path — the N0 spike confirmed
  the hosted-arm64 leg works and is fast (findings under Phase N0), so
  the Orange Pi contingency is closed. Cross-compilation
  stays off the table (native-SDK linking is unproven cross-arch and
  buys nothing while hosted arm64 runners exist).
- **Verification gate: the full lifecycle, per leg, pre-publish.**
  `verify-packages.sh` (Debian), its Fedora leg (N2), `verify-msi.ps1`
  (N3), `verify-brew.sh` (N4). Building without installing is not a gate.
- **Intel macOS is dropped everywhere.** The nightly ships arm64-only
  macOS artifacts, and the stable-channel formulas lose their Intel
  branches when N4 lands.
- **macOS distribution is Homebrew, not a suite `.pkg`.** Rationale
  recorded in the N4 design below: the Windows suite MSI exists because
  Windows has no package manager; with a tap, macOS is Linux-shaped —
  per-service formulas are the feature tree, `brew services` is systemd,
  and a meta-formula gives the one-command family install.
- **Thin workflows, thick scripts.** Every leg drives a repo script
  (`build-packages.sh`, `build-msi.ps1`, the formula generator), so
  on-demand builds, nightly, and the future `release.yml` generalization
  share one logic layer.
- **Failure handling** reuses the open-or-update tracking-issue pattern
  from `scheduled.yml` / `pi-nightly.yml`, label `nightly-packages`.

## Design

### Shared spine (`nightly-packages.yml`, built in N1)

- Triggers: `schedule` (`7 5 * * *` — 05:07 UTC, after pi-nightly at
  04:00, before the 07:07 nightly cluster) + `workflow_dispatch` (added
  in N3: a `dry_run` input that builds + verifies every leg but skips
  publish, for validating a branch's legs without touching the channel).
  `concurrency` group without cancel-in-progress (same reasoning as
  pi-nightly: losing a scheduled run is worse than two back-to-back).
- Job graph:
  1. **`plan`** — checkout, compare `main` HEAD against
     `nightly^{commit}`; if equal, output `skip=true` and every later job
     no-ops. Otherwise compute the base version, date, short SHA and emit
     each dialect as outputs.
  2. **Per-OS build+verify legs** (matrix as phases land) — each stages
     pinned SDKs (with `actions/cache` on `~/.cache/rusty-photon-pkg`
     keyed on the SDK pins, keeping qhyccd.com off the nightly critical
     path), builds via the repo script with the version dialect, runs its
     lifecycle verifier, and uploads the artifacts.
  3. **`publish`** — needs *all* enabled legs; downloads artifacts, writes
     a merged `SHA256SUMS.txt`, force-moves the `nightly` tag to the
     packaged SHA, replaces the prerelease's assets, rewrites the release
     body (SHA, date, asset table). For N4 it also pushes the regenerated
     `-nightly` formulas to the tap in the same job, so tap and assets
     move together.
  4. **`notify-on-failure`** — tracking issue, `schedule` events only.
- Rust build caching per leg via `Swatinem/rust-cache` (packaging builds
  are cargo, not Bazel — same split as today's `build-packages.sh`).

### Phase N0 — tech spike

A `workflow_dispatch`-only scratch workflow on a spike branch, never
merged; findings land as edits to this plan (status table + flagged
unknowns). Checklist, in priority order:

1. **`verify-packages.sh` on `ubuntu-24.04-arm`** — does podman
   `--systemd=always` with the debian:trixie image work on the hosted
   arm64 image the way it does on x86? This is the load-bearing
   assumption of the arm64 leg.
2. **Timings** — cold and warm build+verify wall-clock on both Linux legs
   with the SDK and rust caches wired, plus observed queue latency for
   the arm64 runner pool.
3. **Asset naming** — how `gh release download` and plain `curl` handle
   `+` (and `^`) in asset filenames; pick the convention (keep the
   dialect characters vs a filename-safe dot rendering while package
   metadata keeps the real dialect).
4. **`--deb-version` prototype** — `build-packages.sh` pass-through to
   `cargo deb --deb-version`, then prove `apt` upgrades a `0.1.0-1`
   install to `0.1.0+nightly.…` in a scratch container.
5. **Homebrew version comparison** — confirm
   `0.1.0+nightly.20260712.gabc1234`-shaped strings compare monotonically
   for `brew upgrade` (worst case: dot-render the formula `version`).

**Orange Pi decision (spike output).** Bring up the Orange Pi 5 Ultra as
a dedicated packaging runner **only if** the hosted arm64 leg fails
check 1 or is unacceptable on check 2 (guideline: > 90 min wall-clock or
chronic multi-hour queueing). Trade-offs if adopted: persistent warm
caches and no queue, but packaging needs sudo/apt/podman — a *privileged*
self-hosted runner on a public repo, unlike the deliberately sudo-less
Pi 5 CI runner — so it must copy pi-nightly's security model exactly
(schedule/dispatch-only triggers, never `pull_request`/`push`,
main-pinned checkout), and it couples nightlies to home infrastructure
being up. Default expectation: hosted wins; record the measured numbers
here either way.

**Findings (2026-07-13).** Ran as scratch workflow `spike-n0.yml` on
branch `spike/n0-nightly-packaging` (Actions runs 29216267070 cold,
29216728509 warm; branch deleted after these edits landed). One
mechanical note for N1: a `workflow_dispatch` workflow that exists only
on a non-default branch is not dispatchable — the spike used a
branch-scoped `push` trigger as the dispatch. `nightly-packages.yml`
lands on `main`, so this does not affect it.

1. **Hosted arm64 verify: works.** `verify-packages.sh` (podman
   `--systemd=always`, debian:trixie, the full 17-package
   install → probe → remove → purge lifecycle) passed unmodified on
   `ubuntu-24.04-arm`. podman 4.9.3 is preinstalled on both Linux
   images; nothing to provision beyond the SDKs and cargo-deb.
2. **Timings** (4-vCPU runners; warm = `Swatinem/rust-cache` restores
   deps, workspace crates rebuild — the nightly steady state):

   | Leg | Cold job total | Warm job total | `build-packages.sh` cold / warm | `verify-packages.sh` cold / warm |
   |-----|---------------|----------------|--------------------------------|----------------------------------|
   | `ubuntu-latest` | ~12 min | ~8 min | 565 s / 334 s | 93 s / 108 s |
   | `ubuntu-24.04-arm` | ~11 min | ~7 min | 479 s / 289 s | 117 s / 126 s |

   Observed queue latency ≤ 5 s on both legs in both runs.
3. **Asset naming: settled** (scratch draft release, published
   briefly for the anonymous-curl check, then deleted with its tag).
   `+` survives verbatim in asset names: the API stores the deb
   filename unchanged, the download URL percent-encodes it as `%2B`,
   and anonymous `curl` returns 200 for both the literal-`+` and
   `%2B` forms. `^` does **not** survive: GitHub silently rewrites it
   to `.` at upload (the rpm uploaded as `…-0.1.0^<date>…` was stored
   as `…-0.1.0.<date>…`; the original `^` URL 404s).
   `gh release download` round-trips by stored name with contents
   intact. **Convention**: deb filenames keep `+` as-is; the rpm
   *asset filename* uses the dot rendering `0.1.0.<date>.g<sha>`
   while the rpm *header* keeps the true `^` dialect (rpm/dnf read
   the header, not the filename). The rename must happen in the
   publish path **before** `SHA256SUMS.txt` is generated — otherwise
   the sums file records the `^` name and `sha256sum -c` fails
   against the GitHub-renamed asset.
4. **deb dialect + `--deb-version`: proven** (locally, debian:trixie
   container): dpkg orders `0.1.0-1 < 0.1.0+nightly.<date>.g<sha> <
   0.1.1-1` with monotonic dates; `apt` upgrades a `0.1.0-1` install to
   the nightly in place, refuses the downgrade without
   `--allow-downgrades`, and rolls back cleanly with it. Note for N1:
   `cargo deb --deb-version` uses the string **verbatim** — no `-1`
   revision is appended, so the nightly deb version is revision-less
   (harmless for ordering; keep it that way rather than faking a
   revision).
5. **Homebrew ordering: proven** on real brew (macOS runner): `Version`
   orders the `+nightly` dialect correctly (base < nightly < next
   patch, dates monotonic), so the dot-render fallback is unnecessary.

Early N2 bonus, proven locally the same way: `cargo generate-rpm
--set-metadata 'version = "0.1.0^<date>.g<sha>"'` stamps the `^`
dialect into the rpm header and filename verbatim, and `rpm -U`
upgrades `0.1.0-1` to it and refuses the downgrade ("which is newer
... is already installed").

**Orange Pi decision: NO-GO.** Hosted arm64 passes check 1 outright and
beats the check-2 guideline by an order of magnitude (~11 min cold /
~7 min warm against the 90-min line, with no observed queueing). The
Orange Pi stays out of the nightly path.

### Phase N1 — Debian (the anchor)

- `build-packages.sh` grows `--deb-version <v>` (pass-through to
  `cargo deb --deb-version`, which uses the string verbatim — no `-1`
  revision appended, per the N0 findings; `dist/` path keyed on the
  full version).
- `nightly-packages.yml` lands with the shared spine and two legs:
  `ubuntu-latest` and `ubuntu-24.04-arm` (hosted, per N0), each
  running `build-packages.sh --deb-version …` then `verify-packages.sh`
  (the full install → probe → remove → purge lifecycle).
- Publish job as described in the spine; 17 packages per arch +
  `SHA256SUMS.txt`.
- Docs: nightly-channel section in [docs/packaging.md](../packaging.md) —
  install/upgrade commands, channel semantics, the on-demand-downgrade
  caveat, rollback-by-rebuilding-a-SHA.
- `session-runner` remains the one unpackaged daemon (tracked in
  [service-packaging.md](service-packaging.md)); the nightly ships
  whatever has a `pkg/` dir, so it joins automatically once packaged.

**As built (2026-07-13, PR #508).** Mechanics worth knowing: the publish
job *replaces* assets by deleting all existing ones and re-uploading
(a dropped package must not linger; the brief empty-asset window is
accepted on this channel), and regenerates the merged `SHA256SUMS.txt`
from the downloaded artifacts — the ordering that keeps the N2 rpm
rename ahead of checksumming. The SDK stage cache is keyed on
`hashFiles('scripts/build-packages.sh')` (the script embeds the pins).
The `nightly` tag is pushed lightweight; the skip check peels `^{}` to
also survive a manually created annotated tag. The first post-merge run
(dispatch it manually) creates the release + tag; docs/packaging.md
§Nightly channel documents install/upgrade/rollback, with
`SHA256SUMS.txt` as the stable-URL index to the versioned filenames.

### Phase N2 — Fedora

Building is nearly free — `cargo generate-rpm` is pure Rust and already
wired behind `build-packages.sh --rpm`, so the `.rpm`s build on the same
two Ubuntu legs, both arches. The real work:

- **Versioning**: proven in N0 — `cargo generate-rpm --set-metadata
  'version = "…"'` stamps the `^` dialect into header and filename
  verbatim. The *asset filename* must then be dot-rendered
  (`0.1.0.<date>.g<sha>`) before `SHA256SUMS.txt` is generated, because
  GitHub rewrites `^` to `.` in asset names (N0 findings, item 3); the
  rpm header keeps the true `^` version.
- **Verification**: `verify-packages.sh` is Debian-only today. Add a
  Fedora leg — podman systemd container on a Fedora image, `dnf install`
  → unit active → config self-created → HTTP probe → remove. rpm has no
  purge lifecycle; the manual-cleanup story in `docs/packaging.md`
  already covers it.
- **Carried unknowns** from service-packaging.md land here: the
  rpm package-name override (packages may stay crate-named rather than
  `rusty-photon-<svc>`), and whether rpm dependency auto-resolution is
  adequate across the family.
- **Scope honesty**: no known Fedora consumer today — this phase keeps
  the door open. If none has appeared by the time N2 is picked up,
  consider a shallower verify leg (install + probe, skip the remove
  matrix) and note it here.

**As built (N2):** `build-packages.sh --rpm --deb-version <stamp>`
derives the rpm version from the canonical `+nightly.` stamp itself
(`^` render via `--set-metadata`; thick script, same contract as the
MSI leg — the workflow passes one string). Both carried unknowns
resolved: the package names were already `rusty-photon-<svc>`
(`[package.metadata.generate-rpm]` name, checker-enforced — no override
needed), and dependency auto-resolution is adequate — the builtin
resolver emits decorated soname requires that Fedora provides for the
15 auto-req packages, while the two zwo packages (auto-req disabled;
the SONAME-less blob would poison it) now declare explicit `requires`
mirroring their deb `depends`. The Fedora leg (`verify-packages.sh
--rpm`, fedora:44 systemd container) deliberately preinstalls no
runtime libraries, so its `dnf install` doubles as the
requires-resolution proof; it asserts the scriptlets'
enabled-but-not-started contract before starting units itself, and
verifies erase as remove-not-purge (config + state survive). Found and
fixed on the way: the rpm scriptlets were unguarded — on upgrade the
old `%preun` runs *after* the new `%post`, so every nightly upgrade
would have left the service stopped and disabled; all 17 now carry
`$1` guards (enable on first install only, stop/disable on final erase
only, `try-restart` on upgrade — the deb restart-after-upgrade
equivalent). The full verify leg was kept despite the missing Fedora
consumer because the shallow variant would skip exactly the two proofs
this phase exists for (requires resolution on install, scriptlet
contracts on erase).

### Phase N3 — Windows (after W5)

W5 of [windows-packaging.md](windows-packaging.md) delivers
`build-msi.ps1`, `verify-msi.ps1`, and the release-tag MSI job. Its
"nightly build + verify on a schedule" item is implemented **here**, as
the `msi` leg of `nightly-packages.yml` — not as a scheduler of its own
(coordination note recorded in that plan).

- `build-msi.ps1` gains a version-stamp parameter (ProductVersion
  `0.1.0.<YYDDD>` — the 4th field is display-only in ARP, invisible to
  upgrade logic, which sees `0.1.0`; see the version-dialect table — plus
  the full nightly string for the filename).
- `installer/Package.wxs`: `MajorUpgrade AllowSameVersionUpgrades="yes"`
  (required for nightly-over-nightly, harmless for releases) and the full
  nightly string in ARP comments so Programs & Features can tell
  nightlies apart despite the numeric ProductVersion.
- Leg on `windows-latest`: build → `verify-msi.ps1` (already the full
  lifecycle gate, including the kill-and-observe SCM restart check) →
  upload `rusty-photon-<fullversion>-x64.msi`.
- Nightly-specific check added to `verify-msi.ps1`: download the
  *current* nightly MSI from the release (before assets are replaced),
  install it, then install the freshly built MSI over it — proving the
  `AllowSameVersionUpgrades` upgrade path that release-tag testing never
  exercises. Skip gracefully when no prior nightly exists. (**Suspended
  2026-07-18**, [#582](https://github.com/rusty-photon/rusty-photon/issues/582) —
  see the N3 status row.)

**As built (N3):** `build-msi.ps1 -NightlyVersion <full string>`
validates the stamp against the workspace version and renders the
`<base>.<YYDDD>` ProductVersion itself (thick script; the workflow only
passes the canonical string through — the plan job's output was renamed
`deb_version` → `nightly_version` accordingly, deb consuming it
verbatim). Both preprocessor variables are always defined (`Version` +
`FullVersion`), so releases author their ARP comments the same way. The
upgrade proof lived in `verify-msi.ps1 -UpgradeFrom <prior msi>` (prior
MSI installed first, the main install running as the in-place upgrade,
then a single-surviving-ARP-entry assertion) with the workflow — not the
script — downloading the prior MSI; it was proven live 2026-07-15 and
**removed with the 2026-07-18 suspension**
([#582](https://github.com/rusty-photon/rusty-photon/issues/582)): pre-1.0
config-schema churn reddened it with no product signal. The verifier is
fresh-install only until doctor ships in the packages (D7), when the
proof returns as install prior → upgrade → doctor `--fix` → verify.

### Phase N4 — macOS (Homebrew)

**Why not a suite `.pkg`** (the literal MSI analogue via `productbuild` +
choices XML), and why not a cask wrapping one:

- Unsigned/unnotarized pkgs are blocked outright by Gatekeeper on modern
  macOS (System-Settings-approval dance); we are unsigned pre-1.0, and
  notarization means a paid Developer ID + CI ceremony just to reach the
  install dialog.
- pkg has no uninstall or upgrade management — we would hand-build what
  MSI (`MajorUpgrade`) and dpkg give for free.
- It duplicates the service layer: 17 hand-authored launchd plists plus
  activation logic, versus the one-line `service do` block per formula
  that `brew services` turns into managed launchd services.
- The cask variant fixes none of this: a cask's pkg choices are fixed at
  authoring time (no feature tree for the user) and casks don't
  participate in `brew services`.

The suite MSI exists because Windows has no package manager. Homebrew
*is* the package manager, which puts macOS in the Linux camp: per-service
formulas are the feature tree, and selection moves from install time to
service-start time. The "single installer" experience is a meta-formula:

```sh
brew tap rusty-photon/rusty-photon
brew install rusty-photon                      # meta-formula → whole family
brew services start rusty-photon-zwo-camera    # start what this box uses
brew services start rusty-photon-ui-htmx
```

Config-gated services need no mechanism at all — nothing starts until
`brew services start`, so the gating is inherent.

**Tap layout** (the existing `homebrew-rusty-photon` repo):

- `Formula/rusty-photon-<svc>.rb` × 17 — stable channel, updated by
  `release.yml` on `v*` tags. Absorbs PR-7's rename
  (`filemonitor.rb` → `rusty-photon-filemonitor.rb`, class
  `RustyPhotonFilemonitor`).
- `Formula/rusty-photon-<svc>-nightly.rb` × 17 — nightly channel, updated
  by the nightly publish job, `conflicts_with` its stable sibling (same
  binary names). Channels must be distinct formulas: a formula pins one
  url+sha256, and the stable pointer cannot be overwritten nightly.
- `Formula/rusty-photon.rb` + `rusty-photon-nightly.rb` — meta-formulas
  depending on their channel's 17.

**Binary formulas, not bottles.** Formulas point `url` at per-service
arm64 tarballs on the GitHub release and `bin.install` the binary — the
existing filemonitor pattern. Source-build formulas + bottling would
force `depends_on "rust"` and the native-SDK staging story onto end-user
machines for no benefit. This route also sidesteps signing entirely:
Homebrew's curl download never sets the quarantine xattr, and Rust's
linker ad-hoc-signs arm64 binaries, so brew-installed binaries run with
no notarization — Homebrew is precisely the macOS channel where unsigned
pre-1.0 works smoothly.

**Service model** — each formula carries a `service do` block
(`run`, `keep_alive true`, `log_path`/`error_log_path`,
`run_type :immediate`); `brew services` generates and loads the plist:

| Linux / Windows concept | macOS equivalent |
|---|---|
| systemd unit / SCM service | `service do` block → launchd via `brew services` |
| `Restart=on-failure` + 5 s / SCM failure actions | `keep_alive true` (launchd respawn, ~10 s throttle) — the serial drivers' eager-exit loop carries over |
| `ConditionPathExists=` / demand-start | don't `brew services start` it |
| shared system user / LocalSystem | user-level LaunchAgent by default; `sudo brew services` → boot-time LaunchDaemon for a headless Mac (document both) |
| `/etc/rusty-photon` / `%PROGRAMDATA%` | whatever `rusty-photon-config` resolves on macOS (self-creation unchanged — see flagged unknowns) |
| rolling log files | `log_path` / `error_log_path` under Homebrew's `var/log` |

**Generation.** Versions and sha256s change nightly, so formulas are
stamped by CI — but to keep the explicitness rule meaningful, the
per-service metadata (description, port, service-block particulars) lives
as committed data in this repo; one template renders it with
version/url/sha. The same generator serves the stable channel from
`release.yml` (the PR-7 synergy) and pushes to the tap with the existing
`HOMEBREW_TAP_TOKEN`.

**Nightly leg** (`macos-latest`, arm64): build per-service tarballs —
per-service rather than one family tarball because each formula fetches
its own asset, and versioned filenames keep Homebrew's download cache
coherent — run `verify-brew.sh`, upload. The publish job attaches assets
and pushes the regenerated `-nightly` formulas together.

**`verify-brew.sh`** (the `verify-packages.sh` analogue, pre-publish):
render the formulas with `file://` URLs pointing at the just-built
tarballs → `brew install` → `brew services start` → port probe
(`/management/apiversions` for Alpaca services) → config self-created →
`brew services stop` → uninstall clean. Formulas also carry `test do`
blocks (`--help` probe) for `brew test`.

**Native-SDK services on macOS**: the workspace already builds on macOS
in `bazel.yml`, so SDK provisioning is solved; the packaging-specific
work is the zwo dylib payload/rpath and the QHY firmware story (flagged
unknowns below).

**As built (N4):** `scripts/build-tarballs.sh` (the build-packages.sh
analogue: same services/*/pkg discovery, pinned SDK staging into the same
cache dir, isolated zwo cargo invocations, `--version` validated against
the workspace version — which doubles as release.yml's tag guard) +
`scripts/generate-brew-formulas.sh` (one generator, both channels: the
`--channel` flag picks the `-nightly` suffix; per-service metadata comes
from committed sources — the pkg/ discovery and each crate's Cargo.toml
`description`; the meta-formula's mandatory `url` downloads the channel's
`SHA256SUMS.txt`) + `scripts/verify-brew.sh` (renders `file://` formulas
into a scratch tap; installing the meta-formula is the dependency-wiring
proof; per-service classes mirror verify-packages.sh). Findings that
settled the flagged unknowns: the indi-3rdparty mac arm64 blobs already
carry `@rpath/` install names (no `-id` fixup; binaries get
`@loader_path/../lib` + `@loader_path/lib` rpaths, covering the keg and
an untarred dir), libASICamera2 loads `@rpath/libusb-1.0.0.dylib`
(rewritten at staging to Homebrew's libusb; `depends_on "libusb"` on
zwo-camera + qhy-camera) while libEAFFocuser uses only system frameworks;
`libqhyccd-sys`'s macOS branch gained the `QHYCCD_SDK_DIR` override so the
staged SDK links outside `GITHUB_WORKSPACE`. Stable-channel synergy landed
here as designed: `release.yml`'s macOS job builds the family's arm64
tarballs (Intel dropped) with the same scripts, and `update-homebrew`
regenerates all stable formulas via the generator, retiring the
hand-stamped `filemonitor.rb` (renamed `rusty-photon-filemonitor.rb`; the
formulas are macOS-arm64-only — Linux uses the deb/rpm channel, so the old
`on_linux` branches are gone). Known wart, documented in
docs/packaging-macos.md: rp's self-created config defaults
`session.data_directory` to the Linux path (harmless at startup — the
directory is only created when a session persists).

Dry-run findings (runs 29266671212/29267875377/29269314773/29270658812
on the PR branch — the `dry_run` dispatch built and verified all four
legs without publishing; the fourth run was green across all legs):
16 of 17 services pass the full Homebrew lifecycle on a hosted
macos-latest runner as-is, including `brew services` under launchd and
both cameras serving with the real SDKs. The 17th, **zwo-focuser,
blocks under launchd without a macOS privacy (TCC) grant** — a stack
sample shows the process parked *pre-main* in dyld's static-initializer
phase (the EAF SDK dylib's initializer, which touches HID/Bluetooth
frameworks): process alive, log empty, port never bound, while the
identical binary in a foreground run discovers zero EAFs and serves
within a second. Headless CI cannot click a grant, so verify-brew.sh
holds zwo-focuser to alive-under-launchd (a crash loop still fails)
plus a foreground serve proof, the formula carries a caveat, and
docs/packaging-macos.md documents the grant flow; the exact UX rides
the physical-Mac validation pass. zwo-camera is unaffected (libusb, not
HID). Also proven along the way: Homebrew's tap-trust enforcement
auto-trusts only the formula named on the command line, so the scratch
verify tap needs an explicit `brew trust` before the meta-formula can
pull its dependencies.

### Phase N5 — Debian/Fedora package repositories (Cloudflare)

Today's channel (N1/N2) ships `.deb`/`.rpm` as GitHub release assets: a
machine upgrades by hand (`curl` + `sha256sum -c` +
`apt-get install ./file.deb`, per docs/packaging.md). N5 adds a real
`apt`/`dnf` binary **repository** — `apt update && apt upgrade` /
`dnf upgrade` picks the nightly up automatically — built from the exact
`.deb`/`.rpm` files the `linux` job already builds and
lifecycle-verifies. (Deliberately not called a "source": that term reads
as `deb-src`/source packages in Debian packaging, which this isn't — no
source packages, binary `.deb`/`.rpm` only.) N5 adds no new
package-building logic, only repackaging into repo form + hosting + a
repo-level verification gate.

**Scope: rolling-only, matching the channel's existing shape.** One apt
suite (`nightly`) and one flat dnf repo per arch, always pointing at the
single currently-published nightly version — no accumulating pool, no
multi-version retention. This continues the "one rolling prerelease,
assets replaced, no dated history" decision already fixed for N1-N4
rather than opening a new design axis. It also means `reprepro`/`aptly`'s
incremental-pool machinery buys nothing here: a full stateless regen of
the repo tree from that night's just-built packages mirrors the
"thin workflow, thick script" + "replace, don't accumulate" pattern
`publish` already uses for release assets and the Homebrew tap. Real
multi-version rollback (`apt install pkg=<version>`) is deferred — see
Future considerations.

**Hosting: an R2 bucket on a public custom domain, no Worker.** Unlike
the Bazel remote cache (`tools/bazel-cache-worker/`), this needs no
eviction/LRU logic and no edge-cached content-addressed blobs — just
anonymous-GET static-file serving of a tree that's fully replaced once a
night. R2's own custom-domain public-bucket feature (DNS-provisioning
shape similar to the cache Worker's `routes` block, but attached directly
to the bucket rather than to Worker code in front of it) serves this
directly: `GET`/`HEAD`/`Range`/conditional requests all work natively
against bucket objects, so there's no bespoke serving logic to write or
maintain. New bucket `rusty-photon-packages`, domain
`pkg.rustyphoton.space`, layout:

```
pkg.rustyphoton.space/
  pubkey.asc                                  # repo signing key, public half
  deb/
    dists/nightly/InRelease
    dists/nightly/Release
    dists/nightly/Release.gpg
    dists/nightly/main/binary-amd64/Packages(.gz)
    dists/nightly/main/binary-arm64/Packages(.gz)
    pool/main/*.deb
  rpm/
    x86_64/repodata/  (+ *.rpm)
    aarch64/repodata/ (+ *.rpm)
```

> **As-built note:** the design prose from here through the end of the
> **Workflow integration** subsection records the original plan and is
> historical where it names tooling. The shipped pusher drives R2's
> **S3-compatible endpoint** via the aws CLI, authenticated with the
> same bucket-scoped token's S3 key pair (`PACKAGES_R2_ACCESS_KEY_ID` /
> `PACKAGES_R2_SECRET_ACCESS_KEY`) — Cloudflare's REST API, the only
> API wrangler's object commands can use, turned out to reject
> bucket-scoped object tokens — and `tools/rusty-photon-packages-r2/`
> shipped README-only (no `wrangler.toml`, nothing deploys). Where this
> prose and the **As-built deltas** below disagree, the deltas are
> authoritative.

Writes go straight from CI to R2 via `wrangler r2 object put --remote`
(the same CLI already used to deploy the cache Worker), authenticated
with a new Cloudflare API token scoped to Object Read & Write on just
this bucket (`PACKAGES_R2_API_TOKEN`; the account is already known from
the cache Worker setup). No custom Worker code, no new bearer-token
scheme to invent — reads need no auth (public bucket), writes are gated
purely by the scoped API token staying a CI secret. New
`tools/rusty-photon-packages-r2/` (sibling to `tools/bazel-cache-worker/`)
holds the bucket-provisioning `wrangler.toml` and a README documenting
the one-time setup (create bucket, attach custom domain, mint the scoped
token), matching that tool's own README shape.

**Repo tooling.**

- apt: `dpkg-scanpackages` (per arch, over a pool assembled from that
  night's debs) + a generated `Release` file (`apt-ftparchive release` or
  an equivalent short script — SHA256 of each `Packages`/`Packages.gz`,
  `Suite: nightly`, `Codename: nightly`, `Components: main`,
  `Architectures: amd64 arm64`), signed two ways: `gpg --clearsign` into
  `InRelease` and `gpg --detach-sign --armor` into `Release.gpg` (ship
  both — older `apt` only reads the detached form, current `apt` prefers
  `InRelease`).
- dnf: `createrepo_c` per arch directory, `repomd.xml` detached-signed
  into `repomd.xml.asc`. Unlike apt (which always verifies `Release`/
  `InRelease` against the configured key for any added source), dnf only
  checks `repomd.xml`'s signature when the `.repo` file opts in — the
  file clients install must set `repo_gpgcheck=1` (metadata signature;
  what we sign) and `gpgcheck=0` (individual package signing, which N5
  doesn't do — see the flagged unknown below) alongside `gpgkey=` at
  `pkg.rustyphoton.space/pubkey.asc`. Without `repo_gpgcheck=1`, dnf
  never checks the signature at all and the verify step's "signature
  verifies" proof would not actually be exercising anything.
- New scripts (thin-workflow-thick-script, same contract as
  `build-packages.sh`/`build-tarballs.sh`): `scripts/build-apt-repo.sh`
  (consumes the `linux` job's already-verified `.deb`s, emits the `deb/`
  tree above) and `scripts/build-yum-repo.sh` (same for `.rpm`s → the
  `rpm/` tree). Both take the repo-signing private key via env (imported
  once with `gpg --batch --import`), never written to disk outside the
  runner's ephemeral workspace.

**GPG signing key.** One keypair, generated once, offline, no
passphrase — consistent with every other CI credential in this plan
being a bare secret rather than a secret-plus-passphrase pair; the
GitHub secret store is the sole protection layer, the same trust model
as `HOMEBREW_TAP_TOKEN`. Private key (armored) → new secret
`PACKAGES_GPG_PRIVATE_KEY`. Public key to be committed at
`packaging/gpg/pubkey.asc` (checked in, matching the
`packaging/postinst.common`-style "plain committed files, explicitness
over DRY" convention) *and* re-served byte-for-byte at
`pkg.rustyphoton.space/pubkey.asc` for client convenience — one armored
file, one name, everywhere it's referenced (repo layout, both `.repo`/
`sources` client configs, this section); fingerprint recorded in
docs/packaging.md#nightly-channel next to the existing install
instructions. Key rotation is manual and rare (pre-1.0, single
maintainer) — no automated rotation designed.

**Workflow integration.** New job `repo` in `nightly-packages.yml`, needs
`linux` (both arches' already-verified `.deb`/`.rpm` artifacts), runs
`build-apt-repo.sh` + `build-yum-repo.sh` into a local `site/` tree, then
the verify step below, then uploads `site/` as a build artifact —
mirroring every other leg, it does **not** push to R2 itself. `publish`
gains `repo` in its `needs` list (so a broken repo build blocks the
release exactly like a broken MSI or a broken tap push today) and, after
its existing GitHub-release + Homebrew-tap steps, a final step pushes
`site/` to the bucket. Unlike the Bazel cache's content-addressed keys,
`pool/`/`x86_64/`/`aarch64/` filenames carry the nightly version stamp
(date + short SHA), so a plain put-every-night would leave every prior
night's `.deb`/`.rpm` sitting in the bucket under a distinct key
forever — `dpkg-scanpackages`/`createrepo_c` would then index all of
them, silently turning "rolling" into "accumulating" and breaking the
fixed rolling-only scope above. That still needs a delete pass, but
**not** delete-then-upload: deleting the old tree before the new one is
live would open a window where `apt update`/`dnf makecache` mid-publish
either 404s or fetches a `Release`/`repomd.xml` whose referenced content
partway exists (apt's "Hash Sum mismatch" class of failure). So the
final step orders it upload-then-delete instead: upload the new
`pool/`/`x86_64/`/`aarch64/` content and
`pubkey.asc` first (additive — existing clients are still being served
correctly by the still-live old metadata throughout), then the
top-level metadata last (`Release*`/`InRelease`, `repomd.xml*`) as the
single atomic "flip" moment, and only *after* that succeeds does it
delete whatever's now stale (the previous night's pool/repodata
objects) — the same "replace, don't accumulate" end state as the
GitHub release assets (`gh release delete-asset` in a loop) and the
Homebrew tap formulas (`rm -f homebrew-tap/Formula/*-nightly.rb`)
earlier in this publish job, just reordered so nothing a client might
be mid-fetching ever disappears out from under it. Adding or removing a
service changes which files exist under `pool/`/`x86_64/`/`aarch64/` on
the *next* clean build; `check-pkg-assets.sh` already asserts every
service both plans agree exists.

**Verification (pre-publish, matching the plan's fixed "full lifecycle,
per leg" gate).** A new `scripts/verify-packages-repo.sh`, same shape as
`verify-brew.sh`'s "render against `file://` before touching anything
real": serve the freshly built `site/deb` and `site/rpm` over a local
HTTP server inside the verify step (not yet on R2), import the *public*
half of the signing key into a podman `debian:trixie` / `fedora:44`
container, add the repo (a `sources.list.d` entry / a `.repo` file with
`repo_gpgcheck=1`/`gpgcheck=0`, per the dnf note above) pointing at the
local server, then `apt update && apt install rusty-photon-<svc>` /
`dnf install rusty-photon-<svc>` for at least one service per arch —
proving both that the signature verifies (a client holding only the
public key accepts it — and, on the dnf side, actually checked it, since
the container's `.repo` file carries the same `repo_gpgcheck=1` real
clients would need) and that the package resolves and installs through
the real resolver, not just that the files exist. This is the actual
"does `apt upgrade` work" proof that today's manual curl-and-install
testing never exercises.

**As-built deltas.**

- The publish push is a fourth script, `scripts/push-packages-repo.sh`
  (thin-workflow-thick-script; the inline-step wording above undersold
  it). Ordering as designed — content, metadata flip
  (signature-before-signed within each pair), stale deletes — plus two
  additions. First: wrangler has no `r2 object list`, so the listing is
  itself a bucket object — `manifest.txt` (every live key; pre-written
  as an old∪new union so an interrupted run leaves nothing unlisted).
  Second: ordering alone cannot protect apt's *stable-named* index
  files (a client holding the outgoing `InRelease` fetching a replaced
  `Packages.gz` is a hash mismatch), so the tree publishes
  content-addressed `by-hash/` index copies (`Acquire-By-Hash: yes` in
  Release; apt fetches by the strongest listed hash — SHA512 on
  current apt, verified against a live trixie client) and the pusher
  retains the full previous generation for one publish
  (`manifest-prev.txt`): unique-name objects live exactly two nights,
  so metadata a client just read always resolves. dnf gets the same
  guarantee for free (repodata blobs are hash-named natively).
  Verified by a three-generation publish simulation against a stub
  wrangler — which also caught a locale-mismatch bug (`LC_ALL=C sort`
  vs plain `comm`) that would have killed the first real publish
  mid-run.
- Per-arch consumer proof: the runner's native arch gets the full
  `apt-get install` / `dnf install`; the other arch is proven by
  resolver + signed-checksum download (apt multi-arch
  `apt-get download pkg:arm64`, dnf `download --forcearch`) — no
  emulation, the foreign binary never executes, and per-arch
  scriptlet/unit behavior is already covered arch-natively by
  `verify-packages.sh` in the linux legs.
- The fedora verify client bakes `systemd` into its image: the base
  image ships none, the rpm `%post` calls `systemctl` (exit 127 when
  absent), and dnf5 fails the whole transaction on a scriptlet 127 —
  while every real Fedora host has systemd, where the offline
  `systemctl enable` path succeeds.
- Every object uploads with `Cache-Control: no-store`: Cloudflare's
  default edge-cache extension list covers `.gz`, and a stale cached
  `Packages.gz` against a freshly flipped `InRelease` is a client
  hash-mismatch. Reads origin-pull from R2 (free egress, tiny
  traffic); see tools/rusty-photon-packages-r2/README.md before ever
  adding cache rules.
- Both build scripts fingerprint-check the imported private key
  against the committed `packaging/gpg/pubkey.asc` (`--pubkey`
  overridable for local throwaway-key runs) and die on mismatch, so
  secret/committed-key drift cannot ship an unverifiable tree.
- `tools/rusty-photon-packages-r2/` is README-only: with no Worker
  there is nothing to deploy, so no `wrangler.toml`; bucket + domain
  are two one-time CLI commands documented there.
- The pusher authenticates against R2's **S3-compatible endpoint** (aws
  CLI, preinstalled on ubuntu runners), not `wrangler r2 object` as
  designed above: the first publish attempt (2026-07-16) failed with
  403 because Cloudflare's REST API — the only thing wrangler's object
  commands can drive — rejects bucket-scoped Object Read & Write
  tokens outright (they authenticate solely via the S3 API; REST wants
  an account-wide Admin R2 token, unacceptable blast radius with the
  Bazel-cache bucket in the same account). CI secrets are therefore
  the token's S3 key pair (`PACKAGES_R2_ACCESS_KEY_ID` /
  `PACKAGES_R2_SECRET_ACCESS_KEY`) rather than its REST token value
  (`PACKAGES_R2_API_TOKEN`, retired). S3's `ListObjectsV2` was
  deliberately left unused even though it removes wrangler's
  no-listing constraint: a manifest-driven sweep can only ever touch
  keys the publisher itself wrote. The three-generation publish
  simulation was re-run against a stub `aws` to re-prove the manifest
  retention algebra on the new CLI.

## Verification

- Per-leg lifecycle gates (N1 `verify-packages.sh` both arches, N2 Fedora
  leg, N3 `verify-msi.ps1` incl. the nightly-over-nightly upgrade, N4
  `verify-brew.sh`) run *before* publish; the publish job requires all
  enabled legs.
- First post-merge validation per phase: consume the nightly on a real
  machine — apt-upgrade an arm64 box from `0.1.0-1` to a nightly (N1),
  dnf-upgrade a Fedora box from one nightly to the next (N2; proves the
  guarded scriptlets on a real host), `brew install` + `brew services
  start` on a physical Mac (N4), install a nightly MSI over the previous
  one on the Windows box (N3) — and record results here.
  - **N1 validated on the field rig 2026-07-13**: all 17 packages
    apt-upgraded from `0.1.0-1` to `0.1.0+nightly.20260713.g08484d2` via
    the documented consumer path (`SHA256SUMS.txt` index → `curl` →
    `sha256sum -c` → `apt-get install ./*.deb`). Unit pattern identical
    before and after — the running services restarted onto the new
    binaries and answered their HTTP probes, the serial drivers
    re-handshook against the live hardware, the gated services stayed
    gated, and the two units retry-looping on powered-off devices kept
    retry-looping. All service configs byte-identical across the upgrade.
  - **N2 validated in a Fedora container 2026-07-15** (dev-box podman,
    standing in for a Fedora host — the fleet has no Fedora hardware):
    sentinel nightly→nightly, `0.1.0^20260714.g6b8a1c6-1` →
    `0.1.0^20260715.gb72dc63-1`, via the documented consumer path
    (`SHA256SUMS.txt` → `curl` → `sha256sum -c` → `dnf install`). The
    running unit restarted onto the new binary (`try-restart` postun:
    new MainPID, active), stayed **enabled** — the `$1` scriptlet
    guards held, no stop/disable from the outgoing `%preun` — config
    byte-identical, `/health` 200.
  - **N3 MSI-over-MSI proven by the 2026-07-15 scheduled run** (the
    first with a published-MSI seed): the msi job pulled the prior
    nightly MSI (`…20260714.g6b8a1c6`) and `verify-msi.ps1
    -UpgradeFrom` installed it, then the fresh `…20260715.gb72dc63`
    over it — upgrade OK, single ARP entry, every service class green
    post-upgrade. This proof now recurs on every scheduled run; a
    manual pass on a long-lived Windows box stays optional.
- N5 real-machine validation: point a Debian machine at the apt repo
  and a Fedora machine at the dnf repo per
  docs/packaging.md#nightly-channel, then take a nightly→nightly step
  via plain `apt upgrade` / `dnf upgrade`. **Complete** — results
  below.
  - **First repo publish landed 2026-07-16** (workflow_dispatch run
    after the PR #547 S3-API auth fix; merge commit `5f506ab`): the
    pusher took the first-publish path (both manifests genuinely
    absent), published 100 objects, nothing stale to delete. Live
    surface verified: the served `pubkey.asc` fingerprint matches the
    committed `packaging/gpg/pubkey.asc`, `InRelease` is clearsigned
    and advertises `Acquire-By-Hash: yes`, and the last untested
    seam — `+` in R2 object keys via the custom domain — is closed
    (pool-deb URLs return 200 both literal and `%2B`-encoded).
  - **Live-client installs proven the same day** (podman containers on
    the dev box, documented consumer path, public key only): a
    `debian:trixie` client verified `InRelease` through the
    `signed-by=` keyring and installed sentinel
    `0.1.0+nightly.20260716.g5f506ab` from the pool; a Fedora 44
    client with `repo_gpgcheck=1` built its cache against the signed
    `repomd.xml` and installed `0.1.0^20260716.g5f506ab-1`, unit
    coming out `enabled` (offline enable — the `$1` guards).
  - **Nightly→nightly repo upgrades proven 2026-07-17** against the
    second publish (dispatched run on `30e49ad` — which also
    live-proved the retention algebra: previous manifest read,
    generation retained, zero deletions, 188 objects live = 100 new +
    88 retained generation-1 uniques). The Debian client took plain
    `apt-get upgrade` from `0.1.0+nightly.20260716.g5f506ab` to
    `…20260717.g30e49ad`; a Fedora 44 client seeded from the
    *retained* generation-1 rpm (still served after the generation-2
    flip — the retention guarantee exercised for real) took
    `dnf upgrade` to `0.1.0^20260717.g30e49ad-1` under
    `repo_gpgcheck=1`, with the unit staying `enabled` across the
    upgrade. Operational note: a dnf client inside its
    `metadata_expire` window (48 h default) reports nothing to do
    until the cache expires or `--refresh` is passed — inherent dnf
    caching, acceptable at this channel's cadence.
- The skip-if-unchanged path and the failure-tracking issue get exercised
  naturally within the first week of N1 being live; confirm both behaved
  and note it here.
  - **Failure-tracking issue exercised 2026-07-16**: the first
    post-N5-merge scheduled run failed in `publish` (the wrangler/token
    mismatch recorded in the as-built deltas) and `notify-on-failure`
    filed the tracking issue unprompted — once on the scheduled run and
    again on its rerun after the first issue was closed. Behaved as
    designed. Skip-if-unchanged still pending its first natural
    occurrence.

## Flagged unknowns (resolve during the noted phase)

- [x] (N0) podman `--systemd=always` viability on the `ubuntu-24.04-arm`
      hosted image — **works unmodified**; podman 4.9.3 preinstalled
      (N0 findings, item 1).
- [x] (N0) `+` / `^` in GitHub release asset filenames vs `gh`/`curl` —
      `+` survives verbatim (curl OK literal and `%2B`); `^` is silently
      rewritten to `.` by GitHub at upload. Convention: deb names keep
      `+`; rpm asset filenames dot-render (`0.1.0.<date>.g<sha>`) while
      the rpm header keeps `^`; rename before `SHA256SUMS.txt`
      (N0 findings, item 3).
- [x] (N0) Homebrew version comparison of the nightly dialect — orders
      correctly as-is; the dot-render fallback is unnecessary
      (N0 findings, item 5).
- [x] (N0) Hosted arm64 wall-clock + queue numbers — ~11 min cold /
      ~7 min warm, queue ≤ 5 s → **Orange Pi no-go** (N0 findings).
- [x] (N2) `cargo-generate-rpm` Version override with the `^` dialect —
      `--set-metadata 'version = "…"'` stamps it verbatim; upgrade and
      downgrade-refusal proven with `rpm -U` (N0 findings).
- [x] (N2) rpm package-name override (carried from service-packaging.md) —
      already solved before N2: every service's
      `[package.metadata.generate-rpm]` sets `name = "rusty-photon-<svc>"`
      (checker-enforced), so the rpms were never crate-named. No override
      mechanism needed.
- [x] (N4) zwo dylib payload on macOS — solved with `@loader_path/../lib`
      (keg) + `@loader_path/lib` (untarred dir) rpaths on the binaries; the
      indi-3rdparty mac_arm64 blobs already ship `@rpath/` install names,
      so linking records exactly the reference those rpaths resolve. One
      staging fixup: libASICamera2 loads `@rpath/libusb-1.0.0.dylib`,
      rewritten to Homebrew's libusb (a formula dependency); libEAFFocuser
      references only system frameworks.
- [x] (N4) qhy-camera firmware on macOS — the real look found the mac SDK
      ships **no firmware files at all**: the images are embedded in
      `libqhyccd.a` behind in-process entry points
      (`OSXInitQHYCCDFirmwareArray()` / path-based
      `OSXInitQHYCCDFirmware`), which qhyccd-rs does not bind or call. So
      there is no helper to write and nothing for a formula to install —
      but a cold-plugged camera will not enumerate on a Mac until the
      in-process upload is wired (formula caveat + docs; follow-up:
      bind `OSXInitQHYCCDFirmwareArray` and call it on macOS init,
      validated against a real cold camera).
- [x] (N4) `rusty-photon-config` on macOS resolves
      `~/Library/Application Support/rusty-photon/<svc>.json`
      (`directories::ProjectDirs` — not `~/.config`); blessed in
      docs/packaging-macos.md, including the `sudo brew services` variant
      under `/var/root`.
- [x] (N4) sentinel's watchdog restart on macOS — pure config as hoped:
      `restart_command` is a free-form shell string, so
      `brew services restart rusty-photon-<svc>` slots in (documented in
      docs/packaging-macos.md); live validation rides the first
      physical-Mac pass (Verification).
- [x] (N5) R2 custom-domain public-bucket provisioning mechanics via
      `wrangler` — first-class CLI since wrangler 4.x:
      `wrangler r2 bucket create` + `wrangler r2 bucket domain add
      <bucket> --domain … --zone-id …` (no Worker, no `routes` block);
      documented in tools/rusty-photon-packages-r2/README.md.
- [x] (N5) `apt-ftparchive release` alone **is** sufficient: the
      `-o APT::FTPArchive::Release::{Origin,Label,Suite,Codename,Components,Architectures}`
      options render exactly the small fixed header plus the hash
      blocks; no hand-rolled generator (proven in the implementation
      spike against a real trixie client).
- [x] (N5) Per-*package* signing is unnecessary, now proven rather than
      presumed: tamper tests against real clients showed a modified
      pool `.deb` refused by apt (hash mismatch vs the signed Release
      chain) and a modified `repomd.xml` refused by dnf (bad PGP
      signature) — the signed metadata covers package integrity end to
      end. `gpgcheck=0` + `repo_gpgcheck=1` stays the documented client
      config.
- [x] (N5) apt's `signed-by=` accepts the **armored** `pubkey.asc`
      as-is on trixie (apt ≥ 2.4) — one armored file everywhere, no
      `gpg --dearmor` step, verified by `verify-packages-repo.sh`'s
      real `apt update` in the spike and in every repo-job run since.
- [x] (N5) Stale-object deletion: wrangler (4.94) has **no
      `r2 object list`**, so `push-packages-repo.sh` maintains a
      `manifest.txt` object on the bucket — the previous publish's key
      listing, diffed against the freshly built tree; per-key tolerant
      deletes after the metadata flip, manifest written last, so an
      interrupted publish self-heals on the next run.

## Future considerations

- A rig-update helper consuming the stable nightly asset URLs (the
  rolling-tag design exists partly to enable this).
- Code signing / notarization post-1.0 (Developer ID, winget manifest —
  see windows-packaging.md's future considerations); a notarized suite
  `.pkg` could be revisited then, though the Homebrew model likely stays.
- Dated nightly archives (a second, pruned channel) if bisecting old
  nightlies ever becomes a real need — deliberately excluded from v1.
- Multi-version apt/dnf repositories (real `apt install pkg=<version>`
  rollback) — N5 ships rolling-only; revisit if the on-demand-downgrade
  caveat (N1 Decisions) becomes a real pain point.
- A `stable` suite/repo on the same N5 infrastructure, populated by
  `release.yml` on `v*` tags — N5 covers the nightly channel only; this
  would be a PR-7-shaped follow-up reusing the same bucket, scripts, and
  signing key.

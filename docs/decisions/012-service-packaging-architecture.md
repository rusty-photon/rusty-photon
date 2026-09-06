# ADR-012: System packaging architecture — native `.deb`/`.rpm` for all services

## Status

Accepted (2026-07-04); implementation tracked by
[`docs/plans/archive/service-packaging.md`](../plans/archive/service-packaging.md).
Supersedes the per-service packaging decisions embodied in
[`docs/plans/archive/filemonitor-packaging.md`](../plans/archive/filemonitor-packaging.md)
(package/binary/unit naming, conffile config, per-service user).
Amended by [ADR-015](015-windows-packaging-architecture.md) (2026-07-11):
the "MSI and Homebrew remain filemonitor-only" formats clause is superseded
for Windows — the family ships a single suite MSI. Homebrew/macOS remains
deferred.

## Context

The first deployment target is a Debian 13 arm64 observatory machine running
the full driver family unattended at night (tenets #1/#2). Two questions had
to be settled before packaging could scale beyond filemonitor:

**Containers vs native packages.** Docker was considered and rejected: the
drivers talk to USB cameras and serial adapters (QHY cameras re-enumerate
after a host-side firmware upload), which forces privileged device
passthrough with host-level udev rules and firmware anyway; ASCOM Alpaca
discovery is UDP broadcast (port 32227), which forces `--network host`. A
privileged, host-networked container with host udev/firmware keeps Docker's
daemon and image pipeline while discarding its isolation — the worst
quadrant. Rust's static-ish binaries already deliver the "one artifact"
benefit containers usually buy. systemd + journald is one fewer failure
layer at 2am.

**Does the filemonitor pattern generalize?** Mostly. Three of its choices do
not:

1. *Config as a dpkg conffile in `/etc`.* Six services rewrite their own
   config at runtime (`config.apply` actions and `materialize_identity`
   minting device UUIDs on first start, via `crates/rusty-photon-config`).
   A root-owned conffile blocks those writes; a service-writable conffile
   triggers a conffile prompt on every upgrade. Conffile semantics assume
   the *operator* is the only writer — false here by design (ui-htmx edits
   driver config over HTTP; drivers persist it themselves).
2. *Per-service system users.* At 15 packages this multiplies passwd
   entries and forces divergent maintainer scripts, and rp ↔ plate-solver
   exchange FITS files by absolute path, which same-uid solves trivially.
3. *Bare names* (`filemonitor` package, `/usr/bin/filemonitor`,
   `filemonitor.service`). Generic names in shared namespaces
   (`/usr/bin/rp`!) and no family grouping in apt/dnf.

## Decision

1. **Native packages, cargo tooling.** `.deb` via cargo-deb and `.rpm` via
   cargo-generate-rpm, extending the proven metadata-in-`Cargo.toml` pattern.
   No Bazel packaging layer (`rules_pkg`) — Bazel remains the build/test
   gate; release artifacts stay Cargo-built. Formats for the family are
   deb + rpm only; MSI and Homebrew remain filemonitor-only until a real
   Windows/macOS deployment need exists.
2. **Family naming.** Package `rusty-photon-<svc>`, installed binary
   `/usr/bin/rusty-photon-<svc>` (renamed via asset mapping; Cargo bin names
   unchanged), unit `rusty-photon-<svc>.service`. filemonitor's existing
   bare-named artifacts are renamed — a breaking change accepted pre-1.0.
3. **Config is user-based XDG, owned by the service.** Packages ship no
   config file; units pass no `--config` flag. Services use their existing
   `resolve_config_path` XDG default, which — with the service user's home
   at `/var/lib/rusty-photon` — puts live config at
   `/var/lib/rusty-photon/.config/rusty-photon/<svc>.json`, self-created on
   first start. Packaged behavior is byte-identical to dev behavior, there
   are no conffile/seed/permission mechanics at all, and config is honestly
   modeled as service-owned data. postinst creates an `/etc/rusty-photon`
   symlink to that directory for operator discoverability. `dpkg -P` removes
   the purged service's config + state; the shared user/home/symlink stay.
4. **One shared system user `rusty-photon`** (system account, home
   `/var/lib/rusty-photon`, no shell). Hardware privilege is scoped per
   *unit*, not per user. Serial-class units confer `dialout` (the distro
   default group for tty nodes) plus, on Debian only, `plugdev` (where
   Debian's openocd-class udev rules put FTDI-based serial nodes; the
   group is base-passwd's and is never created on rpm-family hosts, so
   the rpm unit variant stays dialout-only). Camera-class packages ship
   udev rules assigning device nodes to the service account's own
   `rusty-photon` group, so their units need no supplementary groups at
   all. Maintainer scripts stay
   byte-identical across packages (service name derived from
   `$DPKG_MAINTSCRIPT_PACKAGE`), enforced by `scripts/check-pkg-assets.sh`.
5. **Hardened unit template with three service classes** (serial / USB
   camera / network-only), `ProtectSystem=strict` +
   `ReadWritePaths=/var/lib/rusty-photon`, `StateDirectory=rusty-photon/<svc>`;
   `ConfigurationDirectory=` is deliberately avoided (root-owned dir would
   break the config crate's atomic rename). Full template and class matrix
   in the plan.
6. **Asset layout: committed per-service `pkg/` dirs, no generator**, with
   canonical shared maintainer scripts under `packaging/` and a checker
   script asserting convergence. Explicitness wins; `git grep` must find the
   real bytes that ship.
7. **ARM64 packages are built natively on the rig for now**
   (`scripts/build-packages.sh`); CI arm64 packaging is deferred.

## Consequences

- One `apt install rusty-photon-<svc>` per service yields a supervised,
  hardened, auto-restarting daemon whose config appears on first start and
  is editable via ui-htmx or at `/etc/rusty-photon/<svc>.json` (symlink).
- Upgrades never prompt about config and never touch it.
- Every service that can run on defaults self-creates its config on first
  start (the shared `resolve_and_init` bootstrap — identity-minting services
  pass their UniqueID pointers, the rest an empty list); only the
  config-gated services and `session-runner`, which have no usable defaults,
  are operator-provided. One pattern; nothing special-cases.
- Operators looking for config in `/etc` find it (via the symlink), but
  backup tooling must know config lives under `/var/lib/rusty-photon`
  (documented in `docs/packaging.md`).
- sentinel needs a small code fix to adopt `resolve_config_path` (it
  currently ignores XDG entirely) — tracked in the plan, PR-2.
- Renames break any existing install of the old `filemonitor` package and
  the current Homebrew formula/tarball names; `release.yml` and the tap are
  reconciled in the plan's PR-7.
- Editing 15 service `Cargo.toml`s (metadata blocks) changes crate_universe
  manifest hashes → `CARGO_BAZEL_REPIN=1 bazel mod tidy && bazel mod tidy`
  accompanies those PRs.

## References

- Plan (design detail, verification matrix, phases):
  [`docs/plans/archive/service-packaging.md`](../plans/archive/service-packaging.md)
- Native SDK payload policy: [ADR-013](013-native-sdk-payload-policy.md)
- Config machinery this leans on: `crates/rusty-photon-config`
  (`resolve_and_init`, `materialize_identity`, atomic `save()`),
  [`docs/services/config-actions.md`](../services/config-actions.md)
- Service lifecycle (signals, stderr logging, SIGHUP reload):
  [ADR-011](011-error-reporting-layers.md),
  `crates/rusty-photon-service-lifecycle`
- Superseded precedent:
  [`docs/plans/archive/filemonitor-packaging.md`](../plans/archive/filemonitor-packaging.md)

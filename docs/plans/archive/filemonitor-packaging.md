# Filemonitor Packaging Plan

**Status: SUPERSEDED (archived 2026-07-04).** The in-repo packaging work
shipped in PR #33 (`88f2d98`) and proved the cargo-deb / cargo-generate-rpm /
cargo-wix / Homebrew pattern. Family-wide packaging is now driven by
[`service-packaging.md`](../service-packaging.md) per
[ADR-012](../../decisions/012-service-packaging-architecture.md), which
supersedes this plan's per-service decisions (bare `filemonitor`
package/binary/unit names → `rusty-photon-*`; `/etc` conffile config →
user-based XDG; per-service system user → shared `rusty-photon` user). The
two unfinished manual steps below (Homebrew tap repo + `HOMEBREW_TAP_TOKEN`
secret) carry over to that plan's PR-7 phase.

## Goal

Create platform-specific installable packages for the filemonitor service so users can install it without a Rust toolchain.

## Implementation Status

| Phase | Description | Status | Branch / Commit |
|-------|-------------|--------|-----------------|
| Prerequisites | `pkg/` directory, systemd unit, config, maintainer scripts | Done | `feature/filemonitor-packaging` / `a6c9586` |
| Phase 1 | `.deb` package (cargo-deb metadata) | Done | `feature/filemonitor-packaging` / `a6c9586` |
| Phase 2 | `.rpm` package (cargo-generate-rpm metadata) | Done | `feature/filemonitor-packaging` / `a550c36` |
| Phase 3 | `.msi` installer (cargo-wix) | Done | `feature/filemonitor-packaging` / `365500e` |
| Phase 4 | Homebrew formula (release-workflow side) | Done | `feature/filemonitor-packaging` / `365500e` |
| Phase 5 | GitHub Actions release workflow | Done | `feature/filemonitor-packaging` / `365500e` |

All phases merged to `main` in PR #33 (`88f2d98`). The in-repo work —
cargo-deb / cargo-generate-rpm / cargo-wix metadata, the committed
`services/filemonitor/wix/main.wxs` (with config-file placement and
Windows-service registration via `ServiceInstall`/`ServiceControl`), and the
`release.yml` workflow — is complete. Only external GitHub setup remains
before the first tagged release can publish a Homebrew formula:

### Remaining manual steps before first release

- [x] **Phase 3:** `cargo wix init -p filemonitor` → `services/filemonitor/wix/main.wxs` generated, customised, and committed in PR #33
- [ ] **Phase 4:** Create the `ivonnyssen/homebrew-rusty-photon` GitHub repository with an initial `Formula/` directory
- [ ] **Phase 4:** Add a `HOMEBREW_TAP_TOKEN` secret to the main repo (GitHub PAT with `repo` scope on the tap repo)

---

## Prerequisites (Done)

- [x] Create a `pkg/` directory under `services/filemonitor/`
- [x] Create a default `pkg/config.json` (based on existing `examples/config-linux.json`)
- [x] Create a systemd unit file `pkg/filemonitor.service`
- [x] Create deb maintainer scripts `pkg/postinst` and `pkg/postrm`
- [x] Update `docs/services/filemonitor.md` to reflect new packaging support (per CLAUDE.md rule 2)

### systemd unit file (`pkg/filemonitor.service`)

```ini
[Unit]
Description=Filemonitor - ASCOM Alpaca SafetyMonitor
After=network.target

[Service]
Type=simple
ExecStart=/usr/bin/filemonitor --config /etc/filemonitor/config.json
Restart=on-failure
RestartSec=5
User=filemonitor
Group=filemonitor
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
```

### deb post-install script (`pkg/postinst`)

```bash
#!/bin/sh
set -e
if ! getent passwd filemonitor > /dev/null; then
    adduser --system --group --no-create-home --quiet filemonitor
fi
#DEBHELPER#
```

### deb post-remove script (`pkg/postrm`)

```bash
#!/bin/sh
set -e
#DEBHELPER#
```

---

## Phase 1: `.deb` package (Debian/Ubuntu) — Done

**Tool:** [cargo-deb](https://github.com/kornelski/cargo-deb) (v3.6.3)

### What was done

1. Added `[package.metadata.deb]` and `[package.metadata.deb.systemd-units]` to `services/filemonitor/Cargo.toml`
2. Verified locally with `cargo deb -p filemonitor` — produces `target/debian/filemonitor_0.1.0-1_amd64.deb`
3. Inspected package contents and confirmed correct layout:
   - `/usr/bin/filemonitor` — binary
   - `/etc/filemonitor/config.json` — config (marked as conffile)
   - `/usr/lib/systemd/system/filemonitor.service` — systemd unit
   - `postinst` / `postrm` / `prerm` — maintainer scripts with full systemd lifecycle

### Cargo.toml metadata

```toml
[package.metadata.deb]
maintainer = "Igor von Nyssen <igor@vonnyssen.com>"
extended-description = "ASCOM Alpaca SafetyMonitor that monitors file content for astrophotography safety."
section = "science"
priority = "optional"
assets = [
    ["target/release/filemonitor", "usr/bin/", "755"],
    ["pkg/config.json", "etc/filemonitor/config.json", "644"],
]
conf-files = ["/etc/filemonitor/config.json"]
maintainer-scripts = "pkg/"

[package.metadata.deb.systemd-units]
unit-name = "filemonitor"
unit-scripts = "pkg/"
enable = true
start = true
restart-after-upgrade = true
```

### Notes

- `dpkg-shlibdeps` warning is harmless — filemonitor is pure Rust with no system deps.
- Files under `/etc/` are automatically treated as conffiles (preserved on upgrade).
- The `#DEBHELPER#` token in maintainer scripts is replaced by cargo-deb with systemd enable/start/stop logic.
- Asset source paths must use `target/release/` prefix; cargo-deb rewrites internally for cross-compilation.

---

## Phase 2: `.rpm` package (Fedora/RHEL) — Done

**Tool:** [cargo-generate-rpm](https://github.com/cat-in-136/cargo-generate-rpm) (v0.20.0)

### What was done

1. Added `[package.metadata.generate-rpm]` to `services/filemonitor/Cargo.toml`
2. Verified locally with `cargo generate-rpm -p services/filemonitor` — produces `target/generate-rpm/filemonitor-0.1.0-1.x86_64.rpm`
3. Inspected package contents and confirmed correct layout and scriptlets

### Cargo.toml metadata

```toml
[package.metadata.generate-rpm]
summary = "ASCOM Alpaca SafetyMonitor that monitors file content"
license = "MIT OR Apache-2.0"
assets = [
    { source = "target/release/filemonitor", dest = "/usr/bin/filemonitor", mode = "755" },
    { source = "pkg/config.json", dest = "/etc/filemonitor/config.json", mode = "644", config = "noreplace" },
    { source = "pkg/filemonitor.service", dest = "/usr/lib/systemd/system/filemonitor.service", mode = "644" },
]
post_install_script = """
getent passwd filemonitor > /dev/null || useradd -r -s /sbin/nologin filemonitor
systemctl daemon-reload
systemctl enable filemonitor.service
"""
pre_uninstall_script = """
systemctl stop filemonitor.service || true
systemctl disable filemonitor.service || true
"""
post_uninstall_script = "systemctl daemon-reload"
require-sh = true
```

### Notes

- No built-in systemd support; the `.service` file is placed via `assets` and lifecycle managed in scriptlets.
- `config = "noreplace"` preserves user edits on upgrade (saves modifications as `.rpmnew`).
- Does not require `rpmbuild` — generates RPMs directly using the `rpm` crate.
- When building from workspace root, use `-p services/filemonitor` to specify the package directory.

---

## Phase 3: `.msi` installer (Windows) — Partial

**Tool:** [cargo-wix](https://github.com/volks73/cargo-wix) (v0.3.9+)

### What was done

- The release workflow (Phase 5) includes a `build-windows` job that runs `cargo wix -p filemonitor` on `windows-latest`.

### Remaining

- [ ] Run `cargo wix init -p filemonitor` on a Windows machine to generate `services/filemonitor/wix/main.wxs`
- [ ] Optionally customize the WXS to include config file placement and/or Windows Service registration
- [ ] Commit `wix/main.wxs` to the repository

### Notes

- **Windows-only build:** WiX compiler (`candle.exe`) and linker (`light.exe`) are Windows executables; cannot generate the initial WXS template on Linux.
- **WiX v3 only:** cargo-wix does not support WiX v4. GitHub Actions `windows-latest` has WiX 3.14.x pre-installed.
- The generated WXS installs the binary to `Program Files`, creates Start Menu entries, and handles upgrades.
- For Windows Service registration, add `ServiceInstall`/`ServiceControl` elements to the WXS.

---

## Phase 4: Homebrew formula (macOS + Linux) — Partial

### What was done

- The release workflow (Phase 5) includes an `update-homebrew` job that automatically computes SHA256 checksums from the built tarballs and commits an updated formula to the tap repo.

### Remaining

- [ ] Create the GitHub repository: `ivonnyssen/homebrew-rusty-photon`
- [ ] Add an initial `Formula/filemonitor.rb` with placeholder checksums (the release workflow will overwrite it on first tag push)
- [ ] Add a `HOMEBREW_TAP_TOKEN` secret to the main repo (GitHub PAT with `repo` scope on the tap repo)

### Formula template

```ruby
class Filemonitor < Formula
  desc "ASCOM Alpaca SafetyMonitor for astrophotography"
  homepage "https://github.com/rusty-photon/rusty-photon"
  version "0.1.0"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/rusty-photon/rusty-photon/releases/download/v#{version}/filemonitor-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "PLACEHOLDER"
    else
      url "https://github.com/rusty-photon/rusty-photon/releases/download/v#{version}/filemonitor-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "PLACEHOLDER"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/rusty-photon/rusty-photon/releases/download/v#{version}/filemonitor-#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "PLACEHOLDER"
    else
      url "https://github.com/rusty-photon/rusty-photon/releases/download/v#{version}/filemonitor-#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "PLACEHOLDER"
    end
  end

  def install
    bin.install "filemonitor"
  end

  test do
    assert_match "filemonitor", shell_output("#{bin}/filemonitor --help")
  end
end
```

### User installation

```bash
brew tap rusty-photon/rusty-photon
brew install filemonitor
```

### Notes

- Formula class name must be CamelCase of filename.
- No `depends_on` needed for a pure-Rust binary.
- The release archive contains the bare `filemonitor` binary.

---

## Phase 5: GitHub Actions release workflow — Done

**File:** `.github/workflows/release.yml`
**Trigger:** Push of a `v*` tag (e.g., `git tag v0.1.0 && git push origin v0.1.0`)

### Jobs

| Job | Runner | Produces | Details |
|-----|--------|----------|---------|
| `build-linux` | `ubuntu-latest` | `.deb`, `.rpm`, `.tar.gz` (x86_64 + aarch64) | Uses `cargo-deb` and `cargo-generate-rpm` |
| `build-macos` | `macos-latest` | `.tar.gz` (x86_64 + aarch64) | Cross-compiles both architectures |
| `build-windows` | `windows-latest` | `.msi` (x86_64) | Uses `cargo-wix` with pre-installed WiX 3.14.x |
| `release` | `ubuntu-latest` | GitHub Release | Collects all artifacts, generates SHA256SUMS, creates release with auto-generated notes |
| `update-homebrew` | `ubuntu-latest` | Updated formula | Computes checksums from tarballs, commits updated formula to tap repo |

### Secrets required

| Secret | Purpose |
|--------|---------|
| `HOMEBREW_TAP_TOKEN` | GitHub PAT with `repo` scope on `ivonnyssen/homebrew-rusty-photon` |

### Key workflow details

- `cargo deb --no-build --no-strip` reuses the already-built binary instead of rebuilding
- `softprops/action-gh-release@v2` creates the release and uploads all artifacts
- SHA256SUMS file is generated and attached to the release for verification
- Homebrew formula update is committed automatically with a bot identity
- Linux aarch64 cross-compilation uses `gcc-aarch64-linux-gnu` with `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER` env var

---

## Future considerations

- Extend packaging to other services (`ppba-driver`, `qhy-focuser`) once the pattern is proven
- Consider `cargo-dist` as an alternative if maintaining the workflow becomes burdensome
- Add a `launchd` plist for macOS service management (Homebrew `service` integration)
- Investigate static linking via `target x86_64-unknown-linux-musl` for fully self-contained Linux binaries
- Add Windows Service registration to the MSI installer

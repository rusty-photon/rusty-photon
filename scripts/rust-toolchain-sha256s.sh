#!/usr/bin/env bash
# Print the `sha256s` entries for MODULE.bazel's `rust.toolchain` for one
# stable Rust release: usage `scripts/rust-toolchain-sha256s.sh 1.99.0`.
#
# rules_rust pins the archives of the releases it knows (known_shas.bzl); a
# release newer than the rules_rust in use downloads unverified unless
# MODULE.bazel names its hashes. They are taken here from the release's channel
# manifest (`channel-rust-<version>.toml`, the `xz_hash` of each package),
# after checking the manifest's detached signature against the Rust release
# key, whose fingerprint is pinned below rather than trusted from the same
# server that serves the manifest.
#
# Covers the seven tools rules_rust fetches for a host, for the five hosts CI
# and the dev machines run on. Paste the output over the existing entries.
set -euo pipefail

version=${1:?usage: $0 <stable version, e.g. 1.99.0>}
# Rust Language (Tag and Release Signing Key) <rust-key@rust-lang.org>
fingerprint=108F66205EAEB0AAA8DD5E1C85AB96E6FA1BE5FE

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
base=https://static.rust-lang.org
curl -fsSL -o "$work/manifest.toml" "$base/dist/channel-rust-$version.toml"
curl -fsSL -o "$work/manifest.toml.asc" "$base/dist/channel-rust-$version.toml.asc"
curl -fsSL -o "$work/key.asc" "$base/rust-key.gpg.ascii"

mkdir -m 700 "$work/gnupg"
gpg --homedir "$work/gnupg" --quiet --import "$work/key.asc" 2>/dev/null
imported=$(gpg --homedir "$work/gnupg" --with-colons --fingerprint | awk -F: '/^fpr:/ {print $10; exit}')
if [[ "$imported" != "$fingerprint" ]]; then
  echo "rust-key.gpg.ascii has fingerprint $imported, expected $fingerprint" >&2
  exit 1
fi
# --status-fd makes the verdict machine-readable: VALIDSIG names the key that
# signed, so a signature by any other key in the keyring cannot pass. The
# status lines are captured before matching, not piped: `grep -q` exits at
# its first match, and gpg's next status write would then die of SIGPIPE,
# which pipefail reports as a failed verification.
if ! verdict=$(gpg --homedir "$work/gnupg" --status-fd 1 --verify "$work/manifest.toml.asc" "$work/manifest.toml" 2>/dev/null) \
  || ! grep -q "^\[GNUPG:\] VALIDSIG $fingerprint " <<<"$verdict"; then
  echo "channel-rust-$version.toml is not signed by the Rust release key" >&2
  exit 1
fi

python3 -I - "$work/manifest.toml" "$version" <<'EOF'
import sys
import tomllib
from pathlib import PurePosixPath

manifest_path, version = sys.argv[1], sys.argv[2]
# manifest package -> rules_rust tool name (the archive's file-name prefix)
packages = {
    "rustc": "rustc",
    "cargo": "cargo",
    "clippy-preview": "clippy",
    "rustfmt-preview": "rustfmt",
    "llvm-tools-preview": "llvm-tools",
    "rust-std": "rust-std",
    "rust-analyzer-preview": "rust-analyzer",
}
hosts = [
    "aarch64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
]
with open(manifest_path, "rb") as f:
    manifest = tomllib.load(f)
entries = {}
for package, tool in packages.items():
    for host in hosts:
        target = manifest["pkg"][package]["target"].get(host)
        if not target or not target.get("available"):
            sys.exit(f"{package} {version} has no {host} build")
        name = PurePosixPath(target["xz_url"]).name
        if name != f"{tool}-{version}-{host}.tar.xz":
            sys.exit(f"unexpected archive name {name} for {package} on {host}")
        entries[name] = target["xz_hash"]
for name in sorted(entries):
    print(f'        "{name}": "{entries[name]}",')
EOF

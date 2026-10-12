#!/usr/bin/env bash
# Print the `sha256s` entries for MODULE.bazel's `rust.toolchain` for one Rust
# release, either of the two it pins:
#
#   scripts/rust-toolchain-sha256s.sh 1.99.0              # the stable pin
#   scripts/rust-toolchain-sha256s.sh nightly/2026-08-15  # the coverage nightly
#
# rules_rust pins the archives of the releases it knows (known_shas.bzl): the
# stable releases up to its own, and a few nightly dates. Any other release
# downloads unverified unless MODULE.bazel names its hashes. They are taken
# here from the release's channel manifest (the `xz_hash` of each package),
# after checking the manifest's detached signature against the Rust release
# key, whose fingerprint is pinned below rather than trusted from the same
# server that serves the manifest. A nightly's manifest is signed by the same
# key; its rustc version is printed to stderr, to show which cycle it is in.
#
# Covers the seven tools rules_rust fetches for a host, for the five hosts CI
# and the dev machines run on, plus rust-src: one archive for every host,
# which rules_rust fetches for its rust-analyzer toolchain. Paste the output
# over the existing entries.
set -euo pipefail

usage="usage: $0 <stable version, e.g. 1.99.0 | nightly/<date>, e.g. nightly/2026-08-15>"
release=${1:?$usage}
if [[ $release =~ ^nightly/([0-9]{4}-[0-9]{2}-[0-9]{2})$ ]]; then
  date=${BASH_REMATCH[1]}
  manifest=dist/$date/channel-rust-nightly.toml
  # rules_rust names a nightly's archives `<tool>-nightly-<host>.tar.xz` and
  # keys them by date: `<date>/<tool>-nightly-<host>.tar.xz`.
  name_version=nightly
elif [[ $release =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  date=
  manifest=dist/channel-rust-$release.toml
  name_version=$release
else
  echo "$usage" >&2
  exit 2
fi
# Rust Language (Tag and Release Signing Key) <rust-key@rust-lang.org>
fingerprint=108F66205EAEB0AAA8DD5E1C85AB96E6FA1BE5FE

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
base=https://static.rust-lang.org
curl -fsSL -o "$work/manifest.toml" "$base/$manifest"
curl -fsSL -o "$work/manifest.toml.asc" "$base/$manifest.asc"
curl -fsSL -o "$work/key.asc" "$base/rust-key.gpg.ascii"

mkdir -m 700 "$work/gnupg"
gpg --homedir "$work/gnupg" --quiet --import "$work/key.asc" 2>/dev/null
# Captured, then matched, for the reason given at the verdict below: awk
# exits at the first fingerprint, and gpg could still be writing.
keys=$(gpg --homedir "$work/gnupg" --with-colons --fingerprint)
imported=$(awk -F: '/^fpr:/ {print $10; exit}' <<<"$keys")
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
  echo "$manifest is not signed by the Rust release key" >&2
  exit 1
fi

python3 -I - "$work/manifest.toml" "$name_version" "$date" <<'EOF'
import sys
import tomllib
from pathlib import PurePosixPath

manifest_path, version, date = sys.argv[1], sys.argv[2], sys.argv[3]
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
if date:
    if manifest.get("date") != date:
        sys.exit(f"the manifest is dated {manifest.get('date')}, not {date}")
    print(f"nightly/{date} is rustc {manifest['pkg']['rustc']['version']}", file=sys.stderr)
entries = {}
for package, tool in packages.items():
    for host in hosts:
        target = manifest["pkg"][package]["target"].get(host)
        if not target or not target.get("available"):
            sys.exit(f"{package} {version} has no {host} build")
        name = PurePosixPath(target["xz_url"]).name
        if name != f"{tool}-{version}-{host}.tar.xz":
            sys.exit(f"unexpected archive name {name} for {package} on {host}")
        entries[f"{date}/{name}" if date else name] = target["xz_hash"]
target = manifest["pkg"]["rust-src"]["target"].get("*")
if not target or not target.get("available"):
    sys.exit(f"rust-src {version} has no build")
name = PurePosixPath(target["xz_url"]).name
if name != f"rust-src-{version}.tar.xz":
    sys.exit(f"unexpected archive name {name} for rust-src")
entries[f"{date}/{name}" if date else name] = target["xz_hash"]
for key in sorted(entries):
    print(f'        "{key}": "{entries[key]}",')
EOF

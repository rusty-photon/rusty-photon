#!/bin/sh
# build-yum-repo.sh — render the nightly dnf repository trees from the
# already-built-and-verified .rpms (docs/plans/archive/nightly-releases.md, phase
# N5). Consumes the linux legs' dist dir and emits the rpm/ half of the
# static site served at pkg.rustyphoton.space:
#
#   SITE/pubkey.asc                              (signing key, public half)
#   SITE/rpm/<arch>/*.rpm                        (arch = x86_64, aarch64)
#   SITE/rpm/<arch>/repodata/                    (+ repomd.xml.asc)
#
# One flat repo per arch, fully regenerated from scratch on every run —
# no accumulation. Only repomd.xml is signed (detached, armored): dnf
# checks it when the client's .repo sets repo_gpgcheck=1, and the signed
# metadata covers every package via its recorded checksums, so per-package
# signing adds nothing (gpgcheck=0 in the documented client config). The
# rpm filenames are dot-rendered (`^` → `.`) exactly like the GitHub
# release assets; the rpm headers keep the true `^` version.
#
# Signing: PACKAGES_GPG_PRIVATE_KEY in the environment holds the armored
# private key (the CI secret), imported into an ephemeral GNUPGHOME
# removed on exit. SITE/pubkey.asc is the --pubkey file and must match
# the signing key's fingerprint (same drift guard as build-apt-repo.sh).
#
# Usage: scripts/build-yum-repo.sh [--dist DIR] [--site DIR] [--pubkey FILE]
#   --dist DIR     directory with the .rpms (default: dist/<workspace version>)
#   --site DIR     output tree root (default: site)
#   --pubkey FILE  public key to serve and fingerprint-check against the
#                  signing key (default: packaging/gpg/pubkey.asc; point at
#                  a throwaway export for local runs)

set -eu

die() { echo "build-yum-repo: $*" >&2; exit 1; }

usage() {
    sed -n '/^# Usage:/,/^$/{s/^# \{0,1\}//p}' "$0"
}

DIST=""
SITE="site"
PUBKEY="packaging/gpg/pubkey.asc"
while [ $# -gt 0 ]; do
    case "$1" in
        --dist)
            shift
            [ $# -gt 0 ] || die "--dist needs a directory"
            DIST="$1"
            ;;
        --site)
            shift
            [ $# -gt 0 ] || die "--site needs a directory"
            SITE="$1"
            ;;
        --pubkey)
            shift
            [ $# -gt 0 ] || die "--pubkey needs a file"
            PUBKEY="$1"
            ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown option: $1" ;;
    esac
    shift
done

[ -f packaging/postinst.common ] || die "run from the repo root"
[ -n "${PACKAGES_GPG_PRIVATE_KEY:-}" ] || die "PACKAGES_GPG_PRIVATE_KEY is not set (armored private signing key)"
[ -f "$PUBKEY" ] || die "$PUBKEY not found"

if [ -z "$DIST" ]; then
    version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
    DIST="dist/$version"
fi
[ -d "$DIST" ] || die "$DIST not found — run scripts/build-packages.sh --rpm first"

# ---- prerequisites ------------------------------------------------------
SUDO=""
[ "$(id -u)" = 0 ] || SUDO="sudo"
if ! command -v createrepo_c > /dev/null 2>&1; then
    if command -v apt-get > /dev/null 2>&1; then
        echo "Installing prerequisites: createrepo-c"
        $SUDO apt-get update -qq
        $SUDO apt-get install -y --no-install-recommends createrepo-c
    else
        die "createrepo_c not found (Fedora: dnf install createrepo_c)"
    fi
fi
command -v gpg > /dev/null 2>&1 || die "gpg not found"

# ---- signing key --------------------------------------------------------
GNUPGHOME=$(mktemp -d)
export GNUPGHOME
trap 'rm -rf "$GNUPGHOME"' EXIT INT TERM
printf '%s\n' "$PACKAGES_GPG_PRIVATE_KEY" | gpg --batch --quiet --import 2> /dev/null \
    || die "importing PACKAGES_GPG_PRIVATE_KEY failed (expects an armored private key)"

signing_fpr=$(gpg --batch --list-secret-keys --with-colons | awk -F: '/^fpr:/{print $10; exit}')
pubkey_fpr=$(gpg --batch --show-keys --with-colons "$PUBKEY" 2> /dev/null | awk -F: '/^fpr:/{print $10; exit}')
[ -n "$signing_fpr" ] || die "no secret key found after import"
[ "$signing_fpr" = "$pubkey_fpr" ] \
    || die "signing key $signing_fpr does not match $PUBKEY ($pubkey_fpr) — the secret and the committed public key have drifted"

# ---- trees --------------------------------------------------------------
# Arch list from the .rpms actually present (the workflow supplies both
# x86_64 and aarch64; a local single-arch build renders a single-arch tree).
ARCHES=$(for f in "$DIST"/rusty-photon-*.rpm; do
    [ -e "$f" ] || continue
    b=$(basename "$f" .rpm)
    echo "${b##*.}"
done | sort -u | tr '\n' ' ')
[ -n "$ARCHES" ] || die "no rusty-photon-*.rpm packages in $DIST"

echo "Rendering the dnf trees ($SITE/rpm; arches: $ARCHES)"
rm -rf "$SITE/rpm"
mkdir -p "$SITE"
cp "$PUBKEY" "$SITE/pubkey.asc"

for a in $ARCHES; do
    mkdir -p "$SITE/rpm/$a"
    for f in "$DIST"/rusty-photon-*."$a".rpm; do
        # Dot-render `^` in the filename (GitHub-asset convention); the rpm
        # header inside keeps the true `^` version.
        cp "$f" "$SITE/rpm/$a/$(basename "$f" | tr '^' '.')"
    done
    # gz metadata compression: dnf5 defaults are fine with anything, but gz
    # keeps the tree readable by the widest range of clients at no cost.
    createrepo_c --general-compress-type gz "$SITE/rpm/$a" > /dev/null
    gpg --batch --yes --detach-sign --armor \
        -o "$SITE/rpm/$a/repodata/repomd.xml.asc" "$SITE/rpm/$a/repodata/repomd.xml"
done

echo ""
echo "dnf trees in $SITE/rpm (signed by $signing_fpr):"
find "$SITE/rpm" -type f | sort

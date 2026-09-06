#!/bin/sh
# build-apt-repo.sh — render the nightly apt repository tree from the
# already-built-and-verified .debs (docs/plans/archive/nightly-releases.md, phase
# N5). Consumes the linux legs' dist dir and emits the deb/ half of the
# static site served at pkg.rustyphoton.space:
#
#   SITE/pubkey.asc                                  (signing key, public half)
#   SITE/deb/pool/main/*.deb
#   SITE/deb/dists/nightly/{InRelease,Release,Release.gpg}
#   SITE/deb/dists/nightly/main/binary-<arch>/Packages(.gz)
#   SITE/deb/dists/nightly/main/binary-<arch>/by-hash/SHA256/<hash>
#
# One rolling suite (`nightly`), fully regenerated from scratch on every
# run — no pool accumulation, matching the channel's replace-don't-
# accumulate shape. The Release file is signed two ways: clearsigned into
# InRelease (what current apt reads) and detach-signed into Release.gpg
# (what older apt reads). Release advertises `Acquire-By-Hash: yes` and
# the index files are duplicated under content-addressed by-hash/ names:
# together with push-packages-repo.sh's one-generation retention this
# removes the index replacement race entirely — a client that just read
# the outgoing InRelease still fetches the exact index bytes it names.
# (The canonical Packages(.gz) paths stay for by-hash-unaware clients,
# which keep a retry-sized race window instead.) The tree is
# consumer-verified by scripts/verify-packages-repo.sh before anything
# is pushed to the bucket.
#
# Signing: PACKAGES_GPG_PRIVATE_KEY in the environment holds the armored
# private key (the CI secret). It is imported into an ephemeral GNUPGHOME
# removed on exit — never written outside it. The public key clients get
# (SITE/pubkey.asc) is the --pubkey file, and its fingerprint must match
# the signing key: that check catches drift between the committed
# packaging/gpg/pubkey.asc and the secret before an unverifiable tree
# could ship.
#
# Usage: scripts/build-apt-repo.sh [--dist DIR] [--site DIR] [--pubkey FILE]
#   --dist DIR     directory with the .debs (default: dist/<workspace version>)
#   --site DIR     output tree root (default: site)
#   --pubkey FILE  public key to serve and fingerprint-check against the
#                  signing key (default: packaging/gpg/pubkey.asc; point at
#                  a throwaway export for local runs)

set -eu

die() { echo "build-apt-repo: $*" >&2; exit 1; }

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
[ -d "$DIST" ] || die "$DIST not found — run scripts/build-packages.sh first"

# ---- prerequisites ------------------------------------------------------
# dpkg-dev: dpkg-scanpackages; apt-utils: apt-ftparchive; gnupg: signing.
SUDO=""
[ "$(id -u)" = 0 ] || SUDO="sudo"
APT_PKGS="dpkg-dev apt-utils gnupg"
if command -v apt-get > /dev/null 2>&1; then
    missing=""
    for p in $APT_PKGS; do
        dpkg -s "$p" > /dev/null 2>&1 || missing="$missing $p"
    done
    if [ -n "$missing" ]; then
        echo "Installing prerequisites:$missing"
        $SUDO apt-get update -qq
        # shellcheck disable=SC2086 # word-splitting the package list is intended
        $SUDO apt-get install -y --no-install-recommends $missing
    fi
else
    if ! command -v dpkg-scanpackages > /dev/null 2>&1 || ! command -v apt-ftparchive > /dev/null 2>&1; then
        die "apt-get not found and dpkg-scanpackages/apt-ftparchive missing; install equivalents of: $APT_PKGS"
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

# ---- tree ---------------------------------------------------------------
# Arch list from the .debs actually present (the workflow supplies both
# amd64 and arm64; a local single-arch build renders a single-arch tree).
ARCHES=$(for f in "$DIST"/rusty-photon-*.deb; do
    [ -e "$f" ] || continue
    b=$(basename "$f" .deb)
    echo "${b##*_}"
done | sort -u | tr '\n' ' ')
[ -n "$ARCHES" ] || die "no rusty-photon-*.deb packages in $DIST"

echo "Rendering the apt tree ($SITE/deb; arches: $ARCHES)"
rm -rf "$SITE/deb"
mkdir -p "$SITE/deb/pool/main"
cp "$DIST"/rusty-photon-*.deb "$SITE/deb/pool/main/"
cp "$PUBKEY" "$SITE/pubkey.asc"

(
    cd "$SITE/deb"
    for a in $ARCHES; do
        mkdir -p "dists/nightly/main/binary-$a/by-hash/SHA256" \
            "dists/nightly/main/binary-$a/by-hash/SHA512"
        dpkg-scanpackages --arch "$a" pool > "dists/nightly/main/binary-$a/Packages"
        gzip -9kf "dists/nightly/main/binary-$a/Packages"
        # Content-addressed copies for Acquire-By-Hash. apt fetches by the
        # strongest hash the Release below lists — SHA512 on current apt
        # (verified against trixie) — with SHA256 kept for clients that
        # prefer it.
        for idx in Packages Packages.gz; do
            f="dists/nightly/main/binary-$a/$idx"
            cp "$f" "dists/nightly/main/binary-$a/by-hash/SHA256/$(sha256sum "$f" | cut -d' ' -f1)"
            cp "$f" "dists/nightly/main/binary-$a/by-hash/SHA512/$(sha512sum "$f" | cut -d' ' -f1)"
        done
    done
    cd dists/nightly
    apt-ftparchive \
        -o APT::FTPArchive::Release::Origin=rusty-photon \
        -o APT::FTPArchive::Release::Label=rusty-photon \
        -o APT::FTPArchive::Release::Suite=nightly \
        -o APT::FTPArchive::Release::Codename=nightly \
        -o APT::FTPArchive::Release::Components=main \
        -o "APT::FTPArchive::Release::Architectures=$(echo "$ARCHES" | sed 's/ $//')" \
        -o APT::FTPArchive::Release::Acquire-By-Hash=yes \
        release . > Release
    gpg --batch --yes --clearsign -o InRelease Release
    gpg --batch --yes --detach-sign --armor -o Release.gpg Release
)

echo ""
echo "apt tree in $SITE/deb (suite nightly, signed by $signing_fpr):"
find "$SITE/deb" -type f | sort

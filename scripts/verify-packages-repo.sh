#!/bin/sh
# verify-packages-repo.sh — consumer-verify the freshly built apt/dnf
# repository trees BEFORE they are pushed anywhere (docs/plans/
# nightly-releases.md, phase N5): serve SITE over local HTTP, then inside
# podman containers holding only the PUBLIC signing key, add the repo the
# exact way docs/packaging.md tells a real machine to and install through
# the real resolver. This proves what the manual curl-and-install path
# never exercises: the metadata signature verifies for a client that
# only has the public key (apt always checks; dnf checks because the
# .repo carries the same repo_gpgcheck=1 real clients get), and the
# package resolves + installs from the repo index.
#
# Per flavor: a full `apt-get install` / `dnf install` of --service for
# the native architecture, plus a resolver-and-checksum download proof
# for the other architecture (apt multi-arch download / dnf --forcearch)
# when the tree carries it — no emulation needed; the foreign binary is
# never executed. Unit/lifecycle behavior is NOT in scope here: that is
# `cargo ci verify-packages`'s job on the same packages, before this script runs.
#
# Usage: scripts/verify-packages-repo.sh [--site DIR] [--service NAME]
#   --site DIR      the tree build-apt-repo.sh/build-yum-repo.sh rendered
#                   (default: site)
#   --service NAME  the service to install (default: sentinel)

set -eu

die() { echo "verify-packages-repo: $*" >&2; exit 1; }

usage() {
    sed -n '/^# Usage:/,/^$/{s/^# \{0,1\}//p}' "$0"
}

SITE="site"
SERVICE="sentinel"
while [ $# -gt 0 ]; do
    case "$1" in
        --site)
            shift
            [ $# -gt 0 ] || die "--site needs a directory"
            SITE="$1"
            ;;
        --service)
            shift
            [ $# -gt 0 ] || die "--service needs a name"
            SERVICE="$1"
            ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown option: $1" ;;
    esac
    shift
done

# $SERVICE is interpolated into the generated in-container test scripts;
# constrain it to the package-name alphabet so a stray argument fails
# here instead of as shell text inside the container.
case "$SERVICE" in
    "" | *[!a-z0-9-]*) die "--service must be lowercase letters, digits, and hyphens (got: $SERVICE)" ;;
esac

command -v podman > /dev/null 2>&1 || die "podman not found"
command -v python3 > /dev/null 2>&1 || die "python3 not found (serves the tree)"
[ -f "$SITE/pubkey.asc" ] || die "$SITE/pubkey.asc missing — run the build-*-repo.sh scripts first"
SITE_ABS=$(cd "$SITE" && pwd)

case "$(uname -m)" in
    x86_64) DEB_NATIVE=amd64 DEB_FOREIGN=arm64 RPM_NATIVE=x86_64 RPM_FOREIGN=aarch64 ;;
    aarch64) DEB_NATIVE=arm64 DEB_FOREIGN=amd64 RPM_NATIVE=aarch64 RPM_FOREIGN=x86_64 ;;
    *) die "unsupported architecture $(uname -m) (need x86_64 or aarch64)" ;;
esac

TMPD=$(mktemp -d)
HTTP_PID=""
cleanup() {
    [ -n "$HTTP_PID" ] && kill "$HTTP_PID" 2> /dev/null || true
    rm -rf "$TMPD"
}
trap cleanup EXIT INT TERM

# ---- serve the tree -------------------------------------------------------
# 127.0.0.1 only; the containers reach it via --network host. Port 0 lets
# the kernel pick a free one; python prints it on the first line.
python3 -u -m http.server 0 --bind 127.0.0.1 --directory "$SITE_ABS" > "$TMPD/http.log" 2>&1 &
HTTP_PID=$!
i=0
PORT=""
while [ -z "$PORT" ]; do
    PORT=$(sed -n 's/.*port \([0-9]*\).*/\1/p' "$TMPD/http.log" | head -1)
    [ -n "$PORT" ] && break
    i=$((i + 1))
    if [ "$i" -ge 20 ]; then
        # The EXIT trap removes $TMPD, so surface the log now.
        cat "$TMPD/http.log" >&2 || true
        die "local HTTP server did not start (log above)"
    fi
    sleep 0.5
done
BASE="http://127.0.0.1:$PORT"
echo "Serving $SITE_ABS at $BASE (pid $HTTP_PID)"

# ---- apt ------------------------------------------------------------------
if [ -d "$SITE_ABS/deb" ]; then
    # Mounted as a directory over /etc/apt/keyrings (empty in the base
    # image): a directory mount never depends on the runtime creating
    # intermediate paths, and the in-container key path stays exactly the
    # one docs/packaging.md documents.
    mkdir -p "$TMPD/apt-keyrings"
    cp "$SITE_ABS/pubkey.asc" "$TMPD/apt-keyrings/rusty-photon.asc"
    apt_foreign=0
    [ -f "$SITE_ABS/deb/dists/nightly/main/binary-$DEB_FOREIGN/Packages.gz" ] && apt_foreign=1
    cat > "$TMPD/apt-test.sh" <<EOF
set -eu
export DEBIAN_FRONTEND=noninteractive
# The exact client setup docs/packaging.md documents (armored key file).
echo "deb [signed-by=/etc/apt/keyrings/rusty-photon.asc] $BASE/deb nightly main" \
    > /etc/apt/sources.list.d/rusty-photon-nightly.list
apt-get update
apt-get install -y rusty-photon-$SERVICE
dpkg -s rusty-photon-$SERVICE | grep -q "^Status: install ok installed" \
    || { echo "rusty-photon-$SERVICE not installed" >&2; exit 1; }
echo "apt: installed \$(dpkg-query -W -f '\${Version}' rusty-photon-$SERVICE) ($DEB_NATIVE)"
if [ "$apt_foreign" = 1 ]; then
    dpkg --add-architecture $DEB_FOREIGN
    apt-get update > /dev/null
    cd /tmp
    apt-get download rusty-photon-$SERVICE:$DEB_FOREIGN
    ls rusty-photon-${SERVICE}_*_$DEB_FOREIGN.deb > /dev/null
    echo "apt: $DEB_FOREIGN resolved + checksum-verified download OK"
fi
EOF
    echo "== apt: install rusty-photon-$SERVICE from the repo (debian:trixie)"
    podman run --rm --network host \
        -v "$TMPD/apt-keyrings:/etc/apt/keyrings:ro,Z" \
        -v "$TMPD/apt-test.sh:/apt-test.sh:ro,Z" \
        debian:trixie bash /apt-test.sh \
        || die "apt consumer verification failed"
else
    echo "== apt: no $SITE/deb tree — skipped"
fi

# ---- dnf ------------------------------------------------------------------
if [ -d "$SITE_ABS/rpm" ]; then
    # The fedora base image ships no systemd, but every real Fedora host
    # has it — and the rpm %post scriptlets call systemctl, whose absence
    # (exit 127) makes dnf5 fail the whole transaction. Bake the binary in
    # (same reasoning as `cargo ci verify-packages`'s image); no init runs here.
    # dnf5-plugins provides `dnf download` (the foreign-arch proof below);
    # it ships in the current base image, but installing it explicitly
    # keeps the script's dependency real rather than incidental.
    RPM_IMG="localhost/rusty-photon-repo-verify-rpm"
    podman build -q -t "$RPM_IMG" - <<'EOF' > /dev/null
FROM registry.fedoraproject.org/fedora:44
RUN dnf -y install systemd dnf5-plugins && dnf clean all
EOF
    dnf_foreign=0
    [ -d "$SITE_ABS/rpm/$RPM_FOREIGN/repodata" ] && dnf_foreign=1
    cat > "$TMPD/dnf-test.sh" <<EOF
set -eu
# The exact .repo docs/packaging.md documents: repo_gpgcheck=1 makes dnf
# verify repomd.xml's signature (without it dnf never checks at all);
# gpgcheck=0 because packages are covered by the signed metadata's
# checksums, not per-package signatures.
cat > /etc/yum.repos.d/rusty-photon-nightly.repo <<REPO
[rusty-photon-nightly]
name=Rusty Photon nightly
baseurl=$BASE/rpm/\\\$basearch/
enabled=1
repo_gpgcheck=1
gpgcheck=0
gpgkey=$BASE/pubkey.asc
REPO
dnf -y install rusty-photon-$SERVICE
rpm -q rusty-photon-$SERVICE
echo "dnf: installed \$(rpm -q --qf '%{VERSION}' rusty-photon-$SERVICE) ($RPM_NATIVE)"
if [ "$dnf_foreign" = 1 ]; then
    cd /tmp
    dnf -y download --forcearch $RPM_FOREIGN rusty-photon-$SERVICE
    ls rusty-photon-$SERVICE-*.$RPM_FOREIGN.rpm > /dev/null
    echo "dnf: $RPM_FOREIGN resolved + checksum-verified download OK"
fi
EOF
    echo "== dnf: install rusty-photon-$SERVICE from the repo (fedora:44 + systemd)"
    podman run --rm --network host \
        -v "$TMPD/dnf-test.sh:/dnf-test.sh:ro,Z" \
        "$RPM_IMG" bash /dnf-test.sh \
        || die "dnf consumer verification failed"
else
    echo "== dnf: no $SITE/rpm tree — skipped"
fi

echo ""
echo "verify-packages-repo: OK ($BASE, service $SERVICE)"

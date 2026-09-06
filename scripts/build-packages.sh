#!/bin/sh
# build-packages.sh — build the rusty-photon .deb (and optionally .rpm)
# packages natively on the target machine: the Debian arm64 rig or an
# x86_64 box. Operator guide: docs/packaging.md; design:
# docs/plans/archive/service-packaging.md + ADR-012/ADR-013.
#
# Steps:
#   1. install build prerequisites (apt when available; cargo-deb always,
#      cargo-generate-rpm only with --rpm),
#   2. stage the native camera SDKs into ~/.cache/rusty-photon-pkg/:
#      QHYCCD's static libqhyccd.a for the link (exported as
#      QHYCCD_SDK_DIR), and the ZWO MIT blobs each service links (ADR-014:
#      per-device SDK features — zwo-camera → libASICamera2, zwo-focuser →
#      libEAFFocuser). The cache dir doubles as the link path
#      (ZWO_SDK_LIB_DIR); each service's own blob is also copied into its
#      gitignored services/<svc>/pkg/lib/ as that package's payload
#      (ADR-013) — no blob is shared between packages, so the zwo debs
#      co-install without file conflicts. svbony-camera is staged the same
#      way for the *link* (SVBONY_SDK_LIB_DIR) but, per ADR-018 (no license
#      grant at all), its blob is never copied into pkg/lib/ or bundled in
#      the package — the operator downloads it onto the target machine
#      post-install via rusty-photon-svbony-sdk-install instead (see
#      services/svbony-camera/pkg/postinst); the RUNPATH set in step 3
#      below is what lets that operator-installed copy resolve at runtime,
#   3. release-build the selected services with the RUNPATH the zwo-* and
#      svbony-camera packages need (-Wl,-rpath,/usr/lib/rusty-photon;
#      uniform across binaries, harmless where unused), then strip. The two
#      zwo services each build in their OWN cargo invocation: cargo unifies
#      features per invocation, so batching them would re-union the
#      per-device libzwo-sys links and both binaries would need every blob
#      again,
#   4. per service: cargo deb --no-build --no-strip (a rebuild would lose
#      the staged env/RUSTFLAGS); with --rpm also cargo generate-rpm,
#   5. collect artifacts into dist/<version>/ + SHA256SUMS.txt.
#
# Usage: scripts/build-packages.sh [--services a,b,c] [--rpm] [--skip-sdk-staging] [--deb-version V]
#   --services a,b,c    build only these services (default: every packaged one)
#   --rpm               also build .rpm packages (nightly channel + dev-box use)
#   --skip-sdk-staging  offline rebuild: no downloads; requires the SDK
#                       cache from a previous run
#   --deb-version V     stamp V as the .deb version instead of the workspace
#                       version (nightly channel: V like
#                       0.1.0+nightly.202607120507.gabc1234). cargo deb uses
#                       the string verbatim — no -1 revision is appended.
#                       With --rpm, the rpm version is derived from V by
#                       rendering the +nightly. stamp as rpm's ^ snapshot
#                       separator (0.1.0^202607120507.gabc1234), so V must carry
#                       one. Artifacts land in dist/V/.

set -eu

die() { echo "build-packages: $*" >&2; exit 1; }

# ---- pins -------------------------------------------------------------
# The QHY pins must match services/qhy-camera/pkg/rusty-photon-qhy-firmware-install
# (scripts/check-pkg-assets.sh enforces it): the SDK linked at build time and
# the firmware the helper installs on the target must be the same release.
QHY_SDK_VERSION="26.06.04"
QHY_SHA256_X86_64="cbfcec159809e6984c5013a587fed88c892afae9d834019e820213f64616a308"
QHY_SHA256_AARCH64="d28795977311fba1cb7a4fdc48bbd6d5f994716674b2154d9a860d1fdf2f5e0e"
# QHY's download layout for >= 26.06.04: the directory is the dotless version.
QHY_URL_BASE="https://www.qhyccd.com/file/repository/publish/SDK/$(echo "$QHY_SDK_VERSION" | tr -d .)"

# Must match the `ref` default in .github/actions/install-zwo-sdk/action.yml
# (checker-enforced): the blobs shipped in the deb and the blobs CI links
# against must come from the same indi-3rdparty commit. The immutable commit
# SHA in the download URL is the integrity statement, same as in the action.
ZWO_SDK_REF="b0802f28055b67aa6a99580d260c3bb4c27eba4b"

# Must match the `ref` default in .github/actions/install-svbony-sdk/action.yml
# AND the `REF` in services/svbony-camera/pkg/rusty-photon-svbony-sdk-install
# (checker-enforced): the blob this script links against at build time, the
# blob CI links against, and the blob the on-target installer downloads must
# all come from the same indi-3rdparty commit. Same integrity posture as
# ZWO_SDK_REF above — no separate sha256 pin needed here, since this staging
# step (unlike rusty-photon-svbony-sdk-install's real, curled-and-verified
# pin) never ships the blob to an operator machine.
SVBONY_SDK_REF="cd50a3b95032d850cca28d8162513276bc1349ba"

usage() {
    sed -n '/^# Usage:/,/^$/{s/^# \{0,1\}//p}' "$0"
}

RPM=0
SKIP_STAGING=0
ONLY_SERVICES=""
DEB_VERSION=""
while [ $# -gt 0 ]; do
    case "$1" in
        --services)
            shift
            [ $# -gt 0 ] || die "--services needs a comma-separated list"
            ONLY_SERVICES="$1"
            ;;
        --rpm) RPM=1 ;;
        --skip-sdk-staging) SKIP_STAGING=1 ;;
        --deb-version)
            shift
            [ $# -gt 0 ] || die "--deb-version needs a version string"
            DEB_VERSION="$1"
            ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown option: $1" ;;
    esac
    shift
done

[ "$(uname -s)" = Linux ] || die "deb/rpm packages are Linux-only (see release.yml for the other targets)"
[ -f packaging/postinst.common ] || die "run from the repo root"

# The rpm version dialect is derived here, not passed in: the caller hands
# over ONE canonical +nightly string and each packager renders its own
# dialect (same contract as build-msi.ps1). rpm forbids `+` from sorting
# usefully; its `^` separator means exactly "after <base>, before anything
# else" (proven in the N0 spike). Header and filename carry the `^` verbatim.
RPM_VERSION=""
if [ "$RPM" = 1 ] && [ -n "$DEB_VERSION" ]; then
    case "$DEB_VERSION" in
        *+nightly.*) RPM_VERSION=$(printf '%s' "$DEB_VERSION" | sed 's/+nightly\./^/') ;;
        *) die "--rpm with --deb-version needs a +nightly. stamp to derive the rpm version (got: $DEB_VERSION)" ;;
    esac
fi

# ---- service selection --------------------------------------------------
# Packaged services are exactly the crates with a pkg/ dir (same discovery
# as check-pkg-assets.sh). All of them are daemons — phd2-guider gained its
# unit with the HTTP service mode (issue #464).
ALL_SERVICES=$(for d in services/*/pkg; do [ -d "$d" ] && basename "$(dirname "$d")"; done | tr '\n' ' ')

SERVICES=""
if [ -n "$ONLY_SERVICES" ]; then
    for s in $(echo "$ONLY_SERVICES" | tr ',' ' '); do
        case " $ALL_SERVICES " in
            *" $s "*) SERVICES="$SERVICES $s" ;;
            *) die "unknown or unpackaged service: $s (packaged: $ALL_SERVICES)" ;;
        esac
    done
else
    SERVICES="$ALL_SERVICES"
fi

needs_qhy=0
needs_zwo_camera=0
needs_zwo_focuser=0
needs_svbony=0
case " $SERVICES " in *" qhy-camera "*) needs_qhy=1 ;; esac
# libzwo-sys links per device feature (ADR-014): zwo-camera needs only
# libASICamera2, zwo-focuser only libEAFFocuser.
case " $SERVICES " in *" zwo-camera "*) needs_zwo_camera=1 ;; esac
case " $SERVICES " in *" zwo-focuser "*) needs_zwo_focuser=1 ;; esac
case " $SERVICES " in *" svbony-camera "*) needs_svbony=1 ;; esac

case "$(uname -m)" in
    x86_64)
        QHY_FILE="sdk_linux64_${QHY_SDK_VERSION}.tar.gz"
        QHY_SHA256="$QHY_SHA256_X86_64"
        ZWO_ARCH=x64
        # indi-3rdparty's libsvbony blobs use their own arch naming,
        # distinct from libasi's (amd64, not x64) — see
        # install-svbony-sdk/action.yml's "Determine SVBony SDK arch" step.
        SVBONY_ARCH=amd64
        ;;
    aarch64)
        QHY_FILE="sdk_linux_arm64_${QHY_SDK_VERSION}.tar.gz"
        QHY_SHA256="$QHY_SHA256_AARCH64"
        ZWO_ARCH=armv8
        SVBONY_ARCH=armv8
        ;;
    *) die "unsupported architecture $(uname -m) (need x86_64 or aarch64)" ;;
esac

# ---- prerequisites ------------------------------------------------------
SUDO=""
[ "$(id -u)" = 0 ] || SUDO="sudo"

# libusb-1.0-0-dev + libudev-dev: the camera SDK links (-lusb-1.0, -ludev);
# clang/libclang-dev: bindgen in libzwo-sys; dpkg-dev: dpkg-shlibdeps for
# cargo-deb's $auto dependency resolution.
APT_PKGS="build-essential pkg-config curl git ca-certificates dpkg-dev libusb-1.0-0-dev libudev-dev clang libclang-dev"
if command -v apt-get > /dev/null 2>&1; then
    missing=""
    for p in $APT_PKGS; do
        dpkg -s "$p" > /dev/null 2>&1 || missing="$missing $p"
    done
    if [ -n "$missing" ]; then
        echo "Installing build prerequisites:$missing"
        $SUDO apt-get update -qq
        # shellcheck disable=SC2086 # word-splitting the package list is intended
        $SUDO apt-get install -y --no-install-recommends $missing
    fi
else
    echo "build-packages: apt-get not found; make sure equivalents of these are installed: $APT_PKGS" >&2
fi

command -v cargo > /dev/null 2>&1 || die "cargo not found (install Rust via rustup)"
command -v cargo-deb > /dev/null 2>&1 || cargo install --locked cargo-deb
if [ "$RPM" = 1 ]; then
    command -v cargo-generate-rpm > /dev/null 2>&1 || cargo install --locked cargo-generate-rpm
fi

# ---- SDK staging --------------------------------------------------------
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/rusty-photon-pkg"
mkdir -p "$CACHE"

# fetch URL DEST — atomic download (no half-written file poisoning the cache).
fetch() {
    # Explicit check: on non-apt hosts nothing above installed curl, and a
    # bare `curl: not found` under set -e would be low-signal.
    command -v curl > /dev/null 2>&1 || die "curl not found (needed to download the SDKs)"
    echo "Downloading $1"
    curl -fsSL -o "$2.part" "$1"
    mv "$2.part" "$2"
}

if [ "$needs_qhy" = 1 ]; then
    QHY_EXTRACT="$CACHE/qhy-sdk-$QHY_SDK_VERSION-$(uname -m)"
    if [ ! -d "$QHY_EXTRACT" ]; then
        [ "$SKIP_STAGING" = 0 ] || die "--skip-sdk-staging set but $QHY_EXTRACT is missing"
        [ -f "$CACHE/$QHY_FILE" ] || fetch "$QHY_URL_BASE/$QHY_FILE" "$CACHE/$QHY_FILE"
        echo "$QHY_SHA256  $CACHE/$QHY_FILE" | sha256sum -c --status - \
            || die "sha256 mismatch for $QHY_FILE (expected $QHY_SHA256)"
        tmp="$CACHE/extract.$$"
        mkdir -p "$tmp"
        tar -xzf "$CACHE/$QHY_FILE" -C "$tmp"
        mv "$tmp/${QHY_FILE%.tar.gz}" "$QHY_EXTRACT"
        # rm -rf, not rmdir: any extra top-level entry in a future archive
        # layout would make rmdir abort the build under set -e.
        rm -rf "$tmp"
    fi
    # Locate the static lib rather than hardcoding the archive layout.
    qhy_lib=$(find "$QHY_EXTRACT" -name libqhyccd.a | head -1)
    [ -n "$qhy_lib" ] || die "libqhyccd.a not found under $QHY_EXTRACT"
    QHYCCD_SDK_DIR=$(dirname "$qhy_lib")
    export QHYCCD_SDK_DIR
    echo "QHYCCD SDK $QHY_SDK_VERSION staged: QHYCCD_SDK_DIR=$QHYCCD_SDK_DIR"
fi

if [ "$needs_zwo_camera" = 1 ] || [ "$needs_zwo_focuser" = 1 ]; then
    ZWO_CACHE="$CACHE/zwo-$ZWO_SDK_REF-$ZWO_ARCH"
    mkdir -p "$ZWO_CACHE"
    ZWO_BASE="https://github.com/indilib/indi-3rdparty/raw/$ZWO_SDK_REF/libasi"
    zwo_blobs=""
    [ "$needs_zwo_camera" = 1 ] && zwo_blobs="$zwo_blobs libASICamera2"
    [ "$needs_zwo_focuser" = 1 ] && zwo_blobs="$zwo_blobs libEAFFocuser"
    for blob in $zwo_blobs; do
        if [ ! -f "$ZWO_CACHE/$blob.so" ]; then
            [ "$SKIP_STAGING" = 0 ] || die "--skip-sdk-staging set but $ZWO_CACHE/$blob.so is missing"
            fetch "$ZWO_BASE/$ZWO_ARCH/$blob.bin" "$ZWO_CACHE/$blob.so"
        fi
    done
    # The cache dir is the link-search path: it holds (at least) every blob
    # the selected services link, and libzwo-sys's per-device features
    # (ADR-014) mean each service's isolated build looks for exactly its own.
    ZWO_SDK_LIB_DIR="$ZWO_CACHE"
    export ZWO_SDK_LIB_DIR
    echo "ZWO SDK blobs (indi-3rdparty $ZWO_SDK_REF) staged:$zwo_blobs; ZWO_SDK_LIB_DIR=$ZWO_SDK_LIB_DIR"

    # Per-service package payload (ADR-013 + ADR-014): each package ships only
    # the one blob its binary links, so the zwo debs never overlap on a file.
    # Stale extra blobs from pre-ADR-014 runs are removed so pkg/lib/ is
    # exactly the payload.
    if [ "$needs_zwo_camera" = 1 ]; then
        mkdir -p services/zwo-camera/pkg/lib
        rm -f services/zwo-camera/pkg/lib/libEFWFilter.so services/zwo-camera/pkg/lib/libEAFFocuser.so
        cp "$ZWO_CACHE/libASICamera2.so" services/zwo-camera/pkg/lib/
    fi
    if [ "$needs_zwo_focuser" = 1 ]; then
        mkdir -p services/zwo-focuser/pkg/lib
        rm -f services/zwo-focuser/pkg/lib/libASICamera2.so services/zwo-focuser/pkg/lib/libEFWFilter.so
        cp "$ZWO_CACHE/libEAFFocuser.so" services/zwo-focuser/pkg/lib/
    fi
fi

if [ "$needs_svbony" = 1 ]; then
    SVBONY_CACHE="$CACHE/svbony-$SVBONY_SDK_REF-$(uname -m)"
    mkdir -p "$SVBONY_CACHE"
    # Staged under the plain linker name so libsvbony-sys's
    # `-lSVBCameraSDK` resolves it via SVBONY_SDK_LIB_DIR below — mirrors
    # install-svbony-sdk's sudo-free provisioning step exactly (same
    # source, same per-arch blob, same "just the linker name, no SOVERSION
    # dance" reasoning: the vendored .bin carries no embedded SONAME).
    if [ ! -f "$SVBONY_CACHE/libSVBCameraSDK.so" ]; then
        [ "$SKIP_STAGING" = 0 ] || die "--skip-sdk-staging set but $SVBONY_CACHE/libSVBCameraSDK.so is missing"
        fetch "https://github.com/indilib/indi-3rdparty/raw/$SVBONY_SDK_REF/libsvbony/libSVBCameraSDK_${SVBONY_ARCH}.bin" \
            "$SVBONY_CACHE/libSVBCameraSDK.so"
    fi
    # libsvbony-sys/build.rs adds this to its link search path ahead of its
    # /usr/local/lib default (mirrors ZWO_SDK_LIB_DIR). Unlike ZWO, no
    # services/svbony-camera/pkg/lib/ payload copy follows: per ADR-018,
    # SVBony's SDK carries no license grant at all, so this package never
    # bundles it — rusty-photon-svbony-sdk-install downloads it onto the
    # target machine instead (services/svbony-camera/pkg/postinst points
    # the operator at it). This staged copy exists only to satisfy the
    # build-time link; the RUNPATH baked into RUSTFLAGS below is what lets
    # the operator-installed copy resolve at runtime.
    SVBONY_SDK_LIB_DIR="$SVBONY_CACHE"
    export SVBONY_SDK_LIB_DIR
    echo "SVBony SDK (indi-3rdparty $SVBONY_SDK_REF) staged: SVBONY_SDK_LIB_DIR=$SVBONY_SDK_LIB_DIR"
fi

# ---- build ----------------------------------------------------------------
# The RUNPATH lets the SONAME-less bundled ZWO blobs, and the SONAME-less
# operator-installed SVBony blob (rusty-photon-svbony-sdk-install), resolve
# from /usr/lib/rusty-photon at runtime. Deliberately set here, not in a
# build.rs (which would ripple into Bazel/repin). Overrides any ambient
# RUSTFLAGS so the produced binaries do not depend on the invoking shell's
# environment.
RUSTFLAGS="-C link-arg=-Wl,-rpath,/usr/lib/rusty-photon"
export RUSTFLAGS

# The zwo services build in their own cargo invocations: cargo unifies
# features per invocation, so batching zwo-camera (zwo-rs/camera) with
# zwo-focuser (zwo-rs/focuser) would union the per-device libzwo-sys link
# features (ADR-014) and both binaries would link — and need at runtime —
# every SDK blob again. Everything else batches into one invocation.
build_args=""
needs_doctor=0
for s in $SERVICES; do
    case "$s" in
        zwo-camera | zwo-focuser) ;;
        *) build_args="$build_args -p $s" ;;
    esac
    # Sentinel's package carries the doctor binary (no rusty-photon-doctor
    # package exists), so its build needs the doctor crate too.
    [ "$s" = sentinel ] && needs_doctor=1
done
if [ "$needs_doctor" = 1 ]; then
    build_args="$build_args -p doctor"
fi
if [ -n "$build_args" ]; then
    echo "Building release binaries:$build_args"
    # shellcheck disable=SC2086 # word-splitting the -p list is intended
    cargo build --release $build_args
fi
if [ "$needs_zwo_camera" = 1 ]; then
    echo "Building release binaries: -p zwo-camera (isolated: per-device SDK link)"
    cargo build --release -p zwo-camera
fi
if [ "$needs_zwo_focuser" = 1 ]; then
    echo "Building release binaries: -p zwo-focuser (isolated: per-device SDK link)"
    cargo build --release -p zwo-focuser
fi

# Same reasoning as the curl check in fetch(): make a missing binutils on a
# non-apt host fail with an actionable message, not a bare `strip: not found`.
command -v strip > /dev/null 2>&1 || die "strip not found (install binutils)"
for s in $SERVICES; do
    strip "target/release/$s"
done
if [ "$needs_doctor" = 1 ]; then
    strip target/release/doctor
fi

# ---- package -----------------------------------------------------------
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
[ -n "$VERSION" ] || die "could not read the workspace version from Cargo.toml"
DIST="dist/${DEB_VERSION:-$VERSION}"
mkdir -p "$DIST"

for s in $SERVICES; do
    echo "Packaging $s"
    # --no-build: reusing the staged-env build above is essential; a rebuild
    # here would drop QHYCCD_SDK_DIR/ZWO_SDK_LIB_DIR/RUSTFLAGS.
    if [ -n "$DEB_VERSION" ]; then
        cargo deb -p "$s" --no-build --no-strip --deb-version "$DEB_VERSION" --output "$DIST/"
    else
        cargo deb -p "$s" --no-build --no-strip --output "$DIST/"
    fi
    if [ "$RPM" = 1 ]; then
        if [ -n "$RPM_VERSION" ]; then
            cargo generate-rpm -p "services/$s" -o "$DIST/" \
                --set-metadata "version = \"$RPM_VERSION\""
        else
            cargo generate-rpm -p "services/$s" -o "$DIST/"
        fi
    fi
done

(
    cd "$DIST"
    rm -f SHA256SUMS.txt
    # shellcheck disable=SC2035
    sha256sum *.deb *.rpm > SHA256SUMS.txt 2>/dev/null || sha256sum *.deb > SHA256SUMS.txt
)

echo ""
echo "Packages in $DIST/:"
ls -1 "$DIST"

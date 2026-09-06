#!/bin/sh
# build-tarballs.sh — build the per-service macOS arm64 tarballs the Homebrew
# formulas point at (docs/plans/archive/nightly-releases.md N4; operator guide:
# docs/packaging-macos.md). The macOS analogue of build-packages.sh: same
# service discovery (services/*/pkg), same pinned SDK staging, same
# thick-script contract (the workflows only pass the version string through).
#
# Steps:
#   1. check prerequisites (Xcode CLT tools, Homebrew libusb — the QHYCCD
#      static lib and the ZWO camera blob both link it),
#   2. stage the native camera SDKs into ~/.cache/rusty-photon-pkg/:
#      QHYCCD's static libqhyccd.a (exported as QHYCCD_SDK_DIR) and the ZWO
#      MIT dylibs each service links (ADR-014: per-device features —
#      zwo-camera → libASICamera2, zwo-focuser → libEAFFocuser). The mac
#      blobs ship with @rpath/ install names already; the one fixup is
#      libASICamera2's `@rpath/libusb-1.0.0.dylib` load reference, rewritten
#      to Homebrew's libusb so it resolves without shipping libusb ourselves,
#   3. release-build the selected services with @loader_path-relative rpaths
#      (`../lib` for the Homebrew keg layout bin/ → lib/, `lib` for running
#      from an untarred directory; uniform across binaries, harmless where
#      unused). The two zwo services each build in their OWN cargo
#      invocation: cargo unifies features per invocation, so batching them
#      would re-union the per-device libzwo-sys links (ADR-014),
#   4. per service: stage `rusty-photon-<svc>` (+ for the zwo services its
#      one SDK dylib under lib/ and the MIT license) and tar it up,
#   5. collect artifacts into dist/<version>/ + SHA256SUMS.txt.
#
# Usage: scripts/build-tarballs.sh [--services a,b,c] [--skip-sdk-staging] [--version V]
#   --services a,b,c    build only these services (default: every packaged one)
#   --skip-sdk-staging  offline rebuild: no downloads; requires the SDK
#                       cache from a previous run
#   --version V         stamp V into the tarball filenames instead of the
#                       workspace version (nightly channel: V like
#                       0.1.0+nightly.202607130507.gabc1234; must be the workspace
#                       version or carry a +nightly. stamp on it). Artifacts
#                       land in dist/V/.

set -eu

die() { echo "build-tarballs: $*" >&2; exit 1; }

# ---- pins -------------------------------------------------------------
# Must match build-packages.sh's QHY_SDK_VERSION (checker-enforced): the SDK
# a mac binary links and the SDK the Linux packages link must be the same
# release. The sha256 pins the mac arm64 archive specifically.
QHY_SDK_VERSION="26.06.04"
QHY_SHA256_MAC_ARM64="3a03cef52ac95a513e4e7700058352d95c2f0556b02ab15e13efe313168f40a5"
# QHY's download layout for >= 26.06.04: the directory is the dotless version.
QHY_URL_BASE="https://www.qhyccd.com/file/repository/publish/SDK/$(echo "$QHY_SDK_VERSION" | tr -d .)"
QHY_FILE="sdk_mac_arm_${QHY_SDK_VERSION}.tar.gz"

# Must match build-packages.sh and the `ref` default in
# .github/actions/install-zwo-sdk/action.yml (checker-enforced): shipped and
# CI-linked blobs must come from the same indi-3rdparty commit.
ZWO_SDK_REF="b0802f28055b67aa6a99580d260c3bb4c27eba4b"

usage() {
    sed -n '/^# Usage:/,/^$/{s/^# \{0,1\}//p}' "$0"
}

SKIP_STAGING=0
ONLY_SERVICES=""
STAMP_VERSION=""
while [ $# -gt 0 ]; do
    case "$1" in
        --services)
            shift
            [ $# -gt 0 ] || die "--services needs a comma-separated list"
            ONLY_SERVICES="$1"
            ;;
        --skip-sdk-staging) SKIP_STAGING=1 ;;
        --version)
            shift
            [ $# -gt 0 ] || die "--version needs a version string"
            STAMP_VERSION="$1"
            ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown option: $1" ;;
    esac
    shift
done

[ "$(uname -s)" = Darwin ] || die "the tarballs are macOS-only (Linux ships .deb/.rpm — build-packages.sh)"
[ "$(uname -m)" = arm64 ] || die "Intel macOS is not a target (nightly-releases.md: arm64 only)"
[ -f packaging/postinst.common ] || die "run from the repo root"

WORKSPACE_VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
[ -n "$WORKSPACE_VERSION" ] || die "could not read the workspace version from Cargo.toml"

# Same stamp validation as build-msi.ps1: a caller-supplied version must be
# the workspace version itself or a +nightly. stamp on it — anything else
# would silently publish wrongly-versioned artifacts under a plausible name.
VERSION="${STAMP_VERSION:-$WORKSPACE_VERSION}"
case "$VERSION" in
    "$WORKSPACE_VERSION" | "$WORKSPACE_VERSION+nightly."?*) ;;
    *) die "--version must be $WORKSPACE_VERSION or ${WORKSPACE_VERSION}+nightly.<stamp> (got: $VERSION)" ;;
esac

# ---- service selection ----------------------------------------------------
# Packaged services are exactly the crates with a pkg/ dir (same discovery as
# build-packages.sh / check-pkg-assets.sh).
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

# svbony-camera has no confirmed mac_arm64 SVBony SDK blob — indi-3rdparty's
# libsvbony packaging ships mac64/Intel only (see
# .github/actions/install-svbony-sdk/action.yml's header comment), and this
# script is arm64-only (the check above), so there is no real SDK this leg
# could ever link against. Mirrors check-pkg-assets.sh's WIN_SERVICES
# exclusion (same "no SDK for this platform at all" reasoning, a different
# platform) rather than falling back to SVBONY_SKIP_NATIVE_LINK=1: that
# escape hatch produces a simulation-only binary (see libsvbony-sys/build.rs
# + docs/services/svbony-camera.md "Native dependency & build gating"), and
# shipping that as the "real" nightly/release tarball would silently give
# users a driver that can never see a physical camera.
case " $SERVICES " in
    *" svbony-camera "*)
        if [ -n "$ONLY_SERVICES" ]; then
            die "svbony-camera has no confirmed mac_arm64 SVBony SDK blob — cannot build a real-SDK-linked tarball on macOS today (see docs/services/svbony-camera.md Packaging)"
        fi
        echo "Skipping svbony-camera: no confirmed mac_arm64 SVBony SDK blob (indi-3rdparty ships mac64/Intel only)"
        SERVICES=$(echo " $SERVICES " | sed 's/ svbony-camera / /')
        ;;
esac

needs_qhy=0
needs_zwo_camera=0
needs_zwo_focuser=0
case " $SERVICES " in *" qhy-camera "*) needs_qhy=1 ;; esac
case " $SERVICES " in *" zwo-camera "*) needs_zwo_camera=1 ;; esac
case " $SERVICES " in *" zwo-focuser "*) needs_zwo_focuser=1 ;; esac

# ---- prerequisites ------------------------------------------------------
command -v cargo > /dev/null 2>&1 || die "cargo not found (install Rust via rustup)"
for t in strip tar install_name_tool; do
    command -v "$t" > /dev/null 2>&1 || die "$t not found (install the Xcode command line tools)"
done
# macOS ships shasum (perl), not coreutils' sha256sum. Check mode silences
# the per-file OK line via redirection rather than a flag, so it reads the
# same against either implementation; the exit code carries the verdict.
sha256() { shasum -a 256 "$@"; }
sha256_check() { sha256 -c - > /dev/null; }

# Homebrew's libusb: linked by qhy-camera (`-lusb-1.0`) and loaded by the ZWO
# camera blob; the formulas declare `depends_on "libusb"`. This path is the
# TARGET machines' contract, not a build-host detail — it is baked into the
# shipped libASICamera2 load command, and /opt/homebrew is the only supported
# Homebrew prefix on Apple Silicon (what `depends_on "libusb"` provides on
# every standard install; the libqhyccd-sys/libzwo-sys build scripts assume
# it too). Deriving it from the build host's `brew --prefix` would bake a
# nonstandard host layout into the artifact — so a nonstandard host fails
# loudly here instead.
LIBUSB_OPT="/opt/homebrew/opt/libusb"
if [ "$needs_qhy" = 1 ] || [ "$needs_zwo_camera" = 1 ]; then
    if [ ! -e "$LIBUSB_OPT/lib/libusb-1.0.0.dylib" ]; then
        command -v brew > /dev/null 2>&1 || die "Homebrew not found (needed for libusb)"
        echo "Installing libusb via Homebrew"
        brew install libusb
        [ -e "$LIBUSB_OPT/lib/libusb-1.0.0.dylib" ] \
            || die "libusb did not land at $LIBUSB_OPT — a default-prefix (/opt/homebrew) Homebrew is required (the path ships inside the zwo dylib)"
    fi
fi

# ---- SDK staging --------------------------------------------------------
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/rusty-photon-pkg"
mkdir -p "$CACHE"

# fetch URL DEST — atomic download (no half-written file poisoning the cache).
fetch() {
    command -v curl > /dev/null 2>&1 || die "curl not found (needed to download the SDKs)"
    echo "Downloading $1"
    curl -fsSL -o "$2.part" "$1"
    mv "$2.part" "$2"
}

if [ "$needs_qhy" = 1 ]; then
    QHY_EXTRACT="$CACHE/qhy-sdk-$QHY_SDK_VERSION-mac-arm64"
    if [ ! -d "$QHY_EXTRACT" ]; then
        [ "$SKIP_STAGING" = 0 ] || die "--skip-sdk-staging set but $QHY_EXTRACT is missing"
        [ -f "$CACHE/$QHY_FILE" ] || fetch "$QHY_URL_BASE/$QHY_FILE" "$CACHE/$QHY_FILE"
        echo "$QHY_SHA256_MAC_ARM64  $CACHE/$QHY_FILE" | sha256_check \
            || die "sha256 mismatch for $QHY_FILE (expected $QHY_SHA256_MAC_ARM64)"
        tmp="$CACHE/extract.$$"
        mkdir -p "$tmp"
        tar -xzf "$CACHE/$QHY_FILE" -C "$tmp"
        mv "$tmp/${QHY_FILE%.tar.gz}" "$QHY_EXTRACT"
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
    # bindgen (libzwo-sys's build script) needs libclang. Mirror the
    # install-zwo-sdk action: surface Homebrew's llvm via LIBCLANG_PATH,
    # installing it only when neither it nor the Xcode CLT copy (which
    # clang-sys finds on its own) is present.
    if [ -z "${LIBCLANG_PATH:-}" ] && [ ! -e /Library/Developer/CommandLineTools/usr/lib/libclang.dylib ]; then
        command -v brew > /dev/null 2>&1 || die "Homebrew not found (needed for llvm/libclang)"
        [ -d "$(brew --prefix llvm)/lib" ] || { echo "Installing llvm via Homebrew (bindgen needs libclang)"; brew install llvm; }
        LIBCLANG_PATH="$(brew --prefix llvm)/lib"
        export LIBCLANG_PATH
    fi

    ZWO_CACHE="$CACHE/zwo-$ZWO_SDK_REF-mac_arm64"
    mkdir -p "$ZWO_CACHE"
    ZWO_BASE="https://github.com/indilib/indi-3rdparty/raw/$ZWO_SDK_REF/libasi"
    zwo_blobs=""
    [ "$needs_zwo_camera" = 1 ] && zwo_blobs="$zwo_blobs libASICamera2"
    [ "$needs_zwo_focuser" = 1 ] && zwo_blobs="$zwo_blobs libEAFFocuser"
    for blob in $zwo_blobs; do
        if [ ! -f "$ZWO_CACHE/$blob.dylib" ]; then
            [ "$SKIP_STAGING" = 0 ] || die "--skip-sdk-staging set but $ZWO_CACHE/$blob.dylib is missing"
            fetch "$ZWO_BASE/mac_arm64/$blob.bin" "$ZWO_CACHE/$blob.dylib"
        fi
    done
    # The mac blobs ship with @rpath/ install names already (LC_ID_DYLIB
    # @rpath/libASICamera2.dylib / @rpath/libEAFFocuser.dylib), so linking
    # against them records exactly the reference the keg-relative rpaths
    # resolve — no -id fixup needed. The one load-command fixup:
    # libASICamera2 references @rpath/libusb-1.0.0.dylib, which nothing on a
    # target machine provides under our rpaths; rewrite it to Homebrew's
    # libusb (the formula dependency). A no-op on a cache that is already
    # rewritten. libEAFFocuser loads only system frameworks — nothing to fix.
    if [ "$needs_zwo_camera" = 1 ]; then
        install_name_tool -change @rpath/libusb-1.0.0.dylib \
            "$LIBUSB_OPT/lib/libusb-1.0.0.dylib" "$ZWO_CACHE/libASICamera2.dylib"
    fi
    # The cache dir is the link-search path: it holds (at least) every blob
    # the selected services link, and libzwo-sys's per-device features
    # (ADR-014) mean each service's isolated build looks for exactly its own.
    ZWO_SDK_LIB_DIR="$ZWO_CACHE"
    export ZWO_SDK_LIB_DIR
    echo "ZWO SDK dylibs (indi-3rdparty $ZWO_SDK_REF) staged:$zwo_blobs; ZWO_SDK_LIB_DIR=$ZWO_SDK_LIB_DIR"
fi

# ---- build ----------------------------------------------------------------
# Two @loader_path-relative rpaths let the SONAME-equivalent @rpath/ install
# names of the bundled ZWO dylibs resolve from bin/../lib (the Homebrew keg
# after `bin.install` + `lib.install`) or ./lib (an untarred tarball run in
# place). Uniform across binaries, harmless where unused — the same contract
# as build-packages.sh's Linux RUNPATH. Overrides any ambient RUSTFLAGS so
# the produced binaries do not depend on the invoking shell's environment.
RUSTFLAGS="-C link-arg=-Wl,-rpath,@loader_path/../lib -C link-arg=-Wl,-rpath,@loader_path/lib"
export RUSTFLAGS

# The zwo services build in their own cargo invocations: cargo unifies
# features per invocation, so batching zwo-camera (zwo-rs/camera) with
# zwo-focuser (zwo-rs/focuser) would union the per-device libzwo-sys link
# features (ADR-014) and both binaries would link — and need at runtime —
# every SDK dylib again. Everything else batches into one invocation.
build_args=""
needs_doctor=0
for s in $SERVICES; do
    case "$s" in
        zwo-camera | zwo-focuser) ;;
        *) build_args="$build_args -p $s" ;;
    esac
    # Sentinel's tarball carries the doctor binary (no rusty-photon-doctor
    # formula exists), so its build needs the doctor crate too.
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

for s in $SERVICES; do
    strip "target/release/$s"
done
if [ "$needs_doctor" = 1 ]; then
    strip target/release/doctor
fi

# ---- package -----------------------------------------------------------
DIST="dist/$VERSION"
mkdir -p "$DIST"

# No AppleDouble (._*) entries in the tarballs.
COPYFILE_DISABLE=1
export COPYFILE_DISABLE

for s in $SERVICES; do
    echo "Packaging $s"
    stage=$(mktemp -d)
    # The installed-binary rename happens here, same as the deb asset mapping
    # (ADR-012): cargo bin names are unchanged, the tarball ships
    # rusty-photon-<svc>.
    cp "target/release/$s" "$stage/rusty-photon-$s"
    contents="rusty-photon-$s"
    case "$s" in
        sentinel)
            # Doctor rides with sentinel (plan decision 8); the renewal
            # launchd plist is authored by the formula at install time, so
            # the tarball only carries the binary.
            cp target/release/doctor "$stage/rusty-photon-doctor"
            contents="$contents rusty-photon-doctor"
            ;;
        zwo-camera)
            mkdir "$stage/lib"
            cp "$ZWO_CACHE/libASICamera2.dylib" "$stage/lib/"
            cp services/zwo-camera/pkg/ZWO-SDK-LICENSE.txt "$stage/"
            contents="$contents lib ZWO-SDK-LICENSE.txt"
            ;;
        zwo-focuser)
            mkdir "$stage/lib"
            cp "$ZWO_CACHE/libEAFFocuser.dylib" "$stage/lib/"
            cp services/zwo-focuser/pkg/ZWO-SDK-LICENSE.txt "$stage/"
            contents="$contents lib ZWO-SDK-LICENSE.txt"
            ;;
    esac
    # shellcheck disable=SC2086 # word-splitting the content list is intended
    tar -czf "$DIST/rusty-photon-$s-$VERSION-aarch64-apple-darwin.tar.gz" -C "$stage" $contents
    rm -rf "$stage"
done

(
    cd "$DIST"
    rm -f SHA256SUMS.txt
    # shellcheck disable=SC2035
    sha256 *.tar.gz > SHA256SUMS.txt
)

echo ""
echo "Tarballs in $DIST/:"
ls -1 "$DIST"

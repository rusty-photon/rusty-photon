#!/bin/sh
# generate-brew-formulas.sh — render the Homebrew formulas for the
# rusty-photon/homebrew-rusty-photon tap (docs/plans/nightly-releases.md N4;
# operator guide: docs/packaging-macos.md).
#
# One generator serves both channels: release.yml renders the stable
# formulas (rusty-photon-<svc>.rb) on a v* tag, nightly-packages.yml renders
# the -nightly siblings (rusty-photon-<svc>-nightly.rb) on every publish,
# and verify-brew.sh renders either flavor with file:// URLs against
# just-built tarballs. Versions and sha256s change per run, so the formulas
# are stamped by CI — the per-service inputs that make each formula what it
# is stay committed in this repo: the service list (services/*/pkg, the same
# discovery as build-packages.sh), each service's Cargo.toml `description`,
# and the per-service particulars encoded below (libusb dependents, the zwo
# dylib payloads, the qhy-camera firmware caveat).
#
# Per service the formula is a binary formula (no source build): `url`
# points at the per-service arm64 tarball, `bin.install` the binary, a
# `service do` block gives `brew services` the launchd unit (keep_alive =
# the systemd Restart=on-failure parity), and the channels conflict with
# each other (same installed binary names). A meta-formula
# (rusty-photon[-nightly]) depends on the channel's whole family — the
# one-command install; its `url` is the release's SHA256SUMS.txt (a formula
# must download something; the checksum manifest is the natural tiny asset).
#
# Usage: scripts/generate-brew-formulas.sh --channel stable|nightly --version V
#                                          --dist DIR --url-base URL --output DIR
#                                          [--services a,b,c]
#   --channel   stable renders rusty-photon-<svc>.rb, nightly the -nightly set
#   --version   the formula `version` (stable: the release version; nightly:
#               the full +nightly stamp — Homebrew orders it correctly, N0)
#   --dist      directory holding rusty-photon-<svc>-<version>-aarch64-apple-darwin.tar.gz
#               for every generated service plus SHA256SUMS.txt (hashed for
#               the meta-formula)
#   --url-base  where the formulas download from, e.g.
#               https://github.com/rusty-photon/rusty-photon/releases/download/nightly
#               (or file://<dir> for local verification)
#   --output    directory the .rb files are written into
#   --services  generate only these services and skip the meta-formula
#               (verification subsets)

set -eu

die() { echo "generate-brew-formulas: $*" >&2; exit 1; }

usage() {
    sed -n '/^# Usage:/,/^$/{s/^# \{0,1\}//p}' "$0"
}

CHANNEL=""
VERSION=""
DIST=""
URL_BASE=""
OUTPUT=""
ONLY_SERVICES=""
while [ $# -gt 0 ]; do
    case "$1" in
        --channel) shift; [ $# -gt 0 ] || die "--channel needs stable|nightly"; CHANNEL="$1" ;;
        --version) shift; [ $# -gt 0 ] || die "--version needs a version string"; VERSION="$1" ;;
        --dist) shift; [ $# -gt 0 ] || die "--dist needs a directory"; DIST="$1" ;;
        --url-base) shift; [ $# -gt 0 ] || die "--url-base needs a URL"; URL_BASE="$1" ;;
        --output) shift; [ $# -gt 0 ] || die "--output needs a directory"; OUTPUT="$1" ;;
        --services) shift; [ $# -gt 0 ] || die "--services needs a comma-separated list"; ONLY_SERVICES="$1" ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown option: $1" ;;
    esac
    shift
done

case "$CHANNEL" in
    stable) SUFFIX=""; CLASS_SUFFIX=""; SIBLING_SUFFIX="-nightly" ;;
    nightly) SUFFIX="-nightly"; CLASS_SUFFIX="Nightly"; SIBLING_SUFFIX="" ;;
    *) die "--channel must be stable or nightly (got: ${CHANNEL:-nothing})" ;;
esac
[ -n "$VERSION" ] || die "--version is required"
[ -n "$URL_BASE" ] || die "--url-base is required"
[ -n "$OUTPUT" ] || die "--output is required"
[ -n "$DIST" ] || die "--dist is required"
[ -d "$DIST" ] || die "$DIST not found"
[ -f packaging/postinst.common ] || die "run from the repo root"

mkdir -p "$OUTPUT"

# Runs on the ubuntu publish job and on macOS (verify-brew.sh).
sha256_of() {
    if command -v sha256sum > /dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

# Same discovery as build-packages.sh / build-tarballs.sh.
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

# build-tarballs.sh never produces a svbony-camera tarball on macOS (no
# confirmed mac_arm64 SVBony SDK blob — see its own matching exclusion and
# docs/services/svbony-camera.md Packaging), so this default (no
# --services) set must drop it too: rendering a formula pointing at a
# tarball that was never built would fail later, at the missing-file check
# below, with a far less actionable message.
case " $SERVICES " in
    *" svbony-camera "*)
        if [ -n "$ONLY_SERVICES" ]; then
            die "svbony-camera has no macOS tarball (no confirmed mac_arm64 SVBony SDK blob — see docs/services/svbony-camera.md Packaging)"
        fi
        SERVICES=$(echo " $SERVICES " | sed 's/ svbony-camera / /')
        ;;
esac

# rusty-photon-dsd-fp2 -> RustyPhotonDsdFp2 (Homebrew's filename -> class
# convention: capitalize each dash-separated token).
class_of() {
    echo "rusty-photon-$1" | awk -F- '{ for (i = 1; i <= NF; i++) printf "%s%s", toupper(substr($i, 1, 1)), substr($i, 2) }'
}

# The formula desc is the crate description — the committed single source.
desc_of() {
    sed -n 's/^description = "\(.*\)"$/\1/p' "services/$1/Cargo.toml" | head -1
}

# Which services link Homebrew's libusb: qhy-camera links -lusb-1.0
# directly (the QHYCCD static lib's transport), and the ZWO camera blob
# loads @rpath/libusb-1.0.0.dylib (rewritten to the Homebrew opt path at
# tarball build time). The EAF focuser blob uses only system frameworks
# (IOKit HID) — no libusb.
needs_libusb() {
    case "$1" in
        qhy-camera|zwo-camera) return 0 ;;
        *) return 1 ;;
    esac
}

# Which services bundle a ZWO SDK dylib (+ its MIT license) in the tarball.
has_dylib_payload() {
    case "$1" in
        zwo-camera|zwo-focuser) return 0 ;;
        *) return 1 ;;
    esac
}

# The formula license mirrors the PACKAGE license, not just the crate's: the
# zwo tarballs bundle the MIT-only ZWO SDK dylib, so they carry the same
# aggregate expression as their deb metadata ((MIT OR Apache-2.0) AND MIT).
license_line_of() {
    if has_dylib_payload "$1"; then
        echo '  license all_of: [{ any_of: ["MIT", "Apache-2.0"] }, "MIT"]'
    else
        echo '  license any_of: ["MIT", "Apache-2.0"]'
    fi
}

for s in $SERVICES; do
    tarball="rusty-photon-$s-$VERSION-aarch64-apple-darwin.tar.gz"
    [ -f "$DIST/$tarball" ] || die "$DIST/$tarball missing — run scripts/build-tarballs.sh first"
    sha=$(sha256_of "$DIST/$tarball")
    desc=$(desc_of "$s")
    [ -n "$desc" ] || die "$s: no description in services/$s/Cargo.toml"
    class="$(class_of "$s")$CLASS_SUFFIX"
    out="$OUTPUT/rusty-photon-$s$SUFFIX.rb"

    {
        cat <<EOF
# Generated by scripts/generate-brew-formulas.sh — do not edit in the tap.
class $class < Formula
  desc "$desc"
  homepage "https://github.com/rusty-photon/rusty-photon"
  version "$VERSION"
EOF
        license_line_of "$s"
        cat <<EOF
  url "$URL_BASE/$tarball"
  sha256 "$sha"

  depends_on :macos
  depends_on arch: :arm64
EOF
        if needs_libusb "$s"; then
            echo '  depends_on "libusb"'
        fi
        cat <<EOF

  conflicts_with "rusty-photon-$s$SIBLING_SUFFIX",
    because: "both channels install the same rusty-photon-$s binary"

  def install
    bin.install "rusty-photon-$s"
EOF
        if has_dylib_payload "$s"; then
            cat <<EOF
    lib.install Dir["lib/*.dylib"]
    doc.install "ZWO-SDK-LICENSE.txt"
EOF
        fi
        if [ "$s" = sentinel ]; then
            # Doctor rides in this formula (no rusty-photon-doctor formula
            # exists). The renewal plist is authored here, not shipped in
            # the tarball, so its paths are this keg's opt_bin/var; a
            # formula manages one brew service (the sentinel daemon), so
            # the plist is loaded by the operator (see caveats).
            cat <<'EOF'
    bin.install "rusty-photon-doctor"
    (prefix/"rusty-photon-renew.plist").write <<~EOS
      <?xml version="1.0" encoding="UTF-8"?>
      <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
      <plist version="1.0">
      <dict>
        <key>Label</key>
        <string>space.rustyphoton.renew</string>
        <key>ProgramArguments</key>
        <array>
          <string>#{opt_bin}/rusty-photon-doctor</string>
          <string>tls</string>
          <string>renew</string>
        </array>
        <key>StartCalendarInterval</key>
        <dict>
          <key>Hour</key>
          <integer>3</integer>
          <key>Minute</key>
          <integer>0</integer>
        </dict>
        <key>StandardOutPath</key>
        <string>#{var}/log/rusty-photon-renew.log</string>
        <key>StandardErrorPath</key>
        <string>#{var}/log/rusty-photon-renew.log</string>
      </dict>
      </plist>
    EOS
EOF
        fi
        cat <<EOF
  end

  service do
    run [opt_bin/"rusty-photon-$s"]
    keep_alive true
    log_path var/"log/rusty-photon-$s.log"
    error_log_path var/"log/rusty-photon-$s.log"
  end
EOF
        if [ "$s" = qhy-camera ]; then
            cat <<'EOF'

  def caveats
    <<~EOS
      QHYCCD's proprietary SDK is linked statically; nothing to install.
      Caveat: a factory-fresh ("cold") QHY camera needs a one-time firmware
      upload that this build does not yet perform on macOS — connect a
      camera that has been used before (e.g. flashed on a Linux host), or
      see docs/packaging-macos.md in the rusty-photon repo.
    EOS
  end
EOF
        fi
        if [ "$s" = zwo-focuser ]; then
            cat <<'EOF'

  def caveats
    <<~EOS
      EAF discovery uses macOS HID APIs that need a privacy grant for the
      service binary; under `brew services` without one the service blocks
      at startup without serving. Grant it under System Settings → Privacy
      & Security (or run the binary once in a terminal), then restart the
      service. Details: docs/packaging-macos.md in the rusty-photon repo.
    EOS
  end
EOF
        fi
        if [ "$s" = sentinel ]; then
            cat <<'EOF'

  def caveats
    <<~EOS
      This formula also installs rusty-photon-doctor (the diagnosis and
      repair tool; there is no separate doctor formula). To arm the daily
      TLS renewal job — a no-op until `rusty-photon-doctor tls issue` has
      staged certificates — link and load the bundled launchd plist once
      (the opt path stays valid across upgrades, and launchd only
      auto-loads jobs from the LaunchAgents directory):
        ln -sfv #{opt_prefix}/rusty-photon-renew.plist ~/Library/LaunchAgents/
        launchctl bootstrap gui/$UID ~/Library/LaunchAgents/rusty-photon-renew.plist
      Details: docs/packaging-macos.md in the rusty-photon repo.
    EOS
  end
EOF
        fi
        if [ "$s" = sentinel ]; then
            cat <<EOF

  test do
    assert_match "$s", shell_output("#{bin}/rusty-photon-$s --help")
    assert_match "doctor", shell_output("#{bin}/rusty-photon-doctor --help")
  end
end
EOF
        else
            cat <<EOF

  test do
    assert_match "$s", shell_output("#{bin}/rusty-photon-$s --help")
  end
end
EOF
        fi
    } > "$out"
    echo "rendered $out"
done

# The meta-formula: the whole channel in one `brew install`. Skipped for
# --services subsets — a meta depending on unrendered siblings would not
# resolve in a scratch tap.
if [ -z "$ONLY_SERVICES" ]; then
    sums="$DIST/SHA256SUMS.txt"
    [ -f "$sums" ] || die "$sums missing — the meta-formula downloads the checksum manifest"
    sha=$(sha256_of "$sums")
    class="RustyPhoton$CLASS_SUFFIX"
    out="$OUTPUT/rusty-photon$SUFFIX.rb"
    {
        cat <<EOF
# Generated by scripts/generate-brew-formulas.sh — do not edit in the tap.
class $class < Formula
  desc "Rusty Photon astrophotography service family (every service)"
  homepage "https://github.com/rusty-photon/rusty-photon"
  version "$VERSION"
  license any_of: ["MIT", "Apache-2.0"]
  # A formula must download something; the channel's checksum manifest is
  # the natural tiny asset for a meta-formula that only pulls dependencies.
  url "$URL_BASE/SHA256SUMS.txt"
  sha256 "$sha"

  depends_on :macos
  depends_on arch: :arm64
EOF
        for s in $SERVICES; do
            echo "  depends_on \"rusty-photon-$s$SUFFIX\""
        done
        cat <<EOF

  conflicts_with "rusty-photon$SIBLING_SUFFIX",
    because: "both channels install the same service family"

  def install
    prefix.install "SHA256SUMS.txt"
  end

  test do
    assert_path_exists prefix/"SHA256SUMS.txt"
  end
end
EOF
    } > "$out"
    echo "rendered $out"
fi

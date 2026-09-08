#!/bin/sh
# verify-brew.sh — lifecycle-verify the built macOS tarballs through the real
# Homebrew machinery, pre-publish (docs/plans/archive/nightly-releases.md N4). The
# verify-packages.sh analogue: formulas are rendered with file:// URLs
# pointing at the just-built tarballs into a scratch local tap, the
# meta-formula install pulls the whole family (proving the dependency
# wiring), and per service: `brew test` → `brew services start` → HTTP probe
# → config self-created at ~/Library/Application Support/rusty-photon/<svc>.json
# → `brew services stop` → uninstall clean (binary gone; config SURVIVES —
# Homebrew never purges, the rpm-erase parity; manual cleanup is documented
# in docs/packaging-macos.md).
#
# Class exceptions mirror verify-packages.sh: the no-defaultable-config
# services (sky-survey-camera, plate-solver, calibrator-flats) are installed
# but never started — macOS has no ConditionPathExists=; the gate is simply
# not running `brew services start` until a config exists, and starting one
# without a config would keep_alive-respawn-loop by design. The serial
# drivers exit on their absent device, so they verify config +
# handshake-attempted from the service log instead of a probe; the cameras,
# zwo-focuser, and phd2-guider never self-create a config; phd2-guider's
# /health legitimately answers 503 with no PHD2 around. The zwo services
# additionally prove via otool that each binary loads exactly its own
# bundled SDK dylib (ADR-014) — the probe answering is the runtime proof
# that the @loader_path rpath actually resolves it.
#
# Usage: scripts/verify-brew.sh [--services a,b,c] [--dist DIR] [--channel C] [--keep]
#   --services a,b,c  verify only these (default: every service with a
#                     tarball in DIST); skips the meta-formula
#   --dist DIR        tarball directory (default: dist/<workspace version>);
#                     its basename is the version the tarballs are stamped with
#   --channel C       stable or nightly (default: nightly) — which formula
#                     flavor to render and exercise
#   --keep            keep the scratch tap and installed formulas on exit
#                     (debugging)

set -eu

die() { echo "verify-brew: $*" >&2; exit 1; }

usage() {
    sed -n '/^# Usage:/,/^$/{s/^# \{0,1\}//p}' "$0"
}

KEEP=0
DIST=""
ONLY_SERVICES=""
CHANNEL=nightly
while [ $# -gt 0 ]; do
    case "$1" in
        --services)
            shift
            [ $# -gt 0 ] || die "--services needs a comma-separated list"
            ONLY_SERVICES="$1"
            ;;
        --dist)
            shift
            [ $# -gt 0 ] || die "--dist needs a directory"
            DIST="$1"
            ;;
        --channel)
            shift
            [ $# -gt 0 ] || die "--channel needs stable|nightly"
            CHANNEL="$1"
            ;;
        --keep) KEEP=1 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown option: $1" ;;
    esac
    shift
done

case "$CHANNEL" in
    stable) SUFFIX="" ;;
    nightly) SUFFIX="-nightly" ;;
    *) die "--channel must be stable or nightly (got: $CHANNEL)" ;;
esac

[ "$(uname -s)" = Darwin ] || die "Homebrew formulas are verified on macOS only"
# Fail fast where build-tarballs.sh does: the formulas are arm64-only, so an
# Intel Mac would only fail later inside brew install with a murkier error.
[ "$(uname -m)" = arm64 ] || die "the formulas are arm64-only (Intel macOS is not a target)"
[ -f packaging/postinst.common ] || die "run from the repo root"
command -v brew > /dev/null 2>&1 || die "brew not found"

# Non-interactive brew: no auto-update churn, no cleanup passes, no hints.
HOMEBREW_NO_AUTO_UPDATE=1
HOMEBREW_NO_INSTALL_CLEANUP=1
HOMEBREW_NO_ENV_HINTS=1
export HOMEBREW_NO_AUTO_UPDATE HOMEBREW_NO_INSTALL_CLEANUP HOMEBREW_NO_ENV_HINTS

if [ -z "$DIST" ]; then
    version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
    DIST="dist/$version"
fi
[ -d "$DIST" ] || die "$DIST not found — run scripts/build-tarballs.sh first"
DIST_ABS=$(cd "$DIST" && pwd)
# build-tarballs.sh names the dist dir after the version it stamps.
VERSION=$(basename "$DIST_ABS")

# Service list: from the tarballs actually present, or --services.
if [ -n "$ONLY_SERVICES" ]; then
    SERVICES=$(echo "$ONLY_SERVICES" | tr ',' ' ')
else
    SERVICES=$(for f in "$DIST_ABS"/rusty-photon-*-"$VERSION"-aarch64-apple-darwin.tar.gz; do
        [ -e "$f" ] || continue
        b=$(basename "$f")
        b=${b#rusty-photon-}
        echo "${b%-"$VERSION"-aarch64-apple-darwin.tar.gz}"
    done | sort -u | tr '\n' ' ')
fi
[ -n "$SERVICES" ] || die "no rusty-photon-*-$VERSION-aarch64-apple-darwin.tar.gz in $DIST_ABS"
echo "Verifying ($CHANNEL): $SERVICES"

port_of() {
    case "$1" in
        filemonitor) echo 11111 ;;
        ppba-driver) echo 11112 ;;
        qhy-focuser) echo 11113 ;;
        sentinel) echo 11114 ;;
        rp) echo 11115 ;;
        sky-survey-camera) echo 11116 ;;
        star-adventurer-gti) echo 11117 ;;
        pa-falcon-rotator) echo 11118 ;;
        dsd-fp2) echo 11119 ;;
        ui-htmx) echo 11120 ;;
        qhy-camera) echo 11121 ;;
        zwo-camera) echo 11122 ;;
        pa-scops-oag) echo 11123 ;;
        zwo-focuser) echo 11124 ;;
        planetarium-bridge) echo 11126 ;;
        upbv2-driver) echo 11127 ;;
        phd2-guider) echo 11130 ;;
        plate-solver) echo 11131 ;;
        calibrator-flats) echo 11170 ;;
        session-runner) echo 11171 ;;
        polar-align) echo 11172 ;;
        *) echo "" ;;
    esac
}

# Fail fast on missing port mappings, before any install work: checked only
# later, a gap would surface minutes into the lifecycle run, one service at
# a time. check-pkg-assets.sh cross-checks this table against
# verify-packages.sh's, but only when it is run.
missing=""
for s in $SERVICES; do
    [ -n "$(port_of "$s")" ] || missing="$missing $s"
done
[ -z "$missing" ] || die "no port mapping for:$missing — extend port_of()"

probe_path() {
    # Alpaca services answer the management API; the plain-HTTP services
    # (sentinel, rp, ui-htmx, phd2-guider, session-runner, polar-align)
    # expose /health.
    case "$1" in
        sentinel|rp|ui-htmx|phd2-guider|session-runner|polar-align) echo /health ;;
        *) echo /management/apiversions ;;
    esac
}

is_gated() {
    # No defaultable config. There is no launchd ConditionPathExists=
    # equivalent and none is needed: nothing runs until
    # `brew services start`, so the gate is not starting them (a start
    # without a config exits and keep_alive respawn-loops by design).
    case "$1" in
        sky-survey-camera|plate-solver|calibrator-flats|session-runner|polar-align) return 0 ;;
        *) return 1 ;;
    esac
}

is_serial() {
    # Serial drivers exit on the absent device (deliberate: never advertise
    # a broken device); launchd keep_alive respawns them, the launchd
    # equivalent of the systemd 5s retry loop.
    case "$1" in
        ppba-driver|upbv2-driver|qhy-focuser|pa-falcon-rotator|pa-scops-oag|dsd-fp2|star-adventurer-gti) return 0 ;;
        *) return 1 ;;
    esac
}

self_creates_config() {
    # Same contract as verify-packages.sh: the cameras, zwo-focuser, and
    # phd2-guider run on built-in defaults and write no file; gated services
    # never start without one.
    if is_gated "$1"; then return 1; fi
    case "$1" in
        qhy-camera|zwo-camera|zwo-focuser|phd2-guider) return 1 ;;
        *) return 0 ;;
    esac
}

is_hid_tcc_gated() {
    # EAF discovery is USB-HID (IOHIDManager); under a launchd agent without
    # a macOS privacy (TCC) grant it blocks before the server ever binds —
    # the process sits alive with an empty log (proven in the N4 dry runs).
    # Headless CI cannot click a grant, so the launchd probe is replaced by:
    # alive-under-launchd (a crash loop is still a regression) + a foreground
    # serve proof. zwo-camera is unaffected (libusb, not HID).
    case "$1" in
        zwo-focuser) return 0 ;;
        *) return 1 ;;
    esac
}

# Verification needs a clean slate (the fresh-container parity): the scratch
# formulas share keg names with the verified channel's real installs and
# conflict with the sibling channel's, so any pre-existing install would
# either get verified — and uninstalled by cleanup — in place of the
# just-built tarballs, or fail later on conflicts_with with a murkier error.
# Refuse both channels up front; this also makes every uninstall below
# provably target something this script installed.
preinstalled=$(brew list --formula 2> /dev/null || true)
for s in $SERVICES rusty-photon; do
    for sfx in "" "-nightly"; do
        name="rusty-photon-$s$sfx"
        [ "$s" = rusty-photon ] && name="rusty-photon$sfx"
        if printf '%s\n' "$preinstalled" | grep -qx "$name"; then
            die "$name is already installed — uninstall it before verifying (the scratch-tap formulas share its keg names or conflict with it)"
        fi
    done
done

# ---- scratch tap ----------------------------------------------------------
TAP="local/rp-verify"
TAP_DIR="$(brew --repository)/Library/Taps/local/homebrew-rp-verify"
PREFIX=$(brew --prefix)
# What rusty-photon-config resolves on macOS (directories::ProjectDirs).
CFG_DIR="$HOME/Library/Application Support/rusty-photon"

STARTED=""
cleanup() {
    if [ "$KEEP" = 1 ]; then
        echo "Kept: tap $TAP (remove: brew untap $TAP) and any installed formulas"
        return
    fi
    for s in $STARTED; do
        brew services stop "rusty-photon-$s$SUFFIX" > /dev/null 2>&1 || true
    done
    # Meta first: brew refuses to uninstall a dependency of an installed
    # formula, so the services only come out after their dependent is gone.
    brew uninstall --force "rusty-photon$SUFFIX" > /dev/null 2>&1 || true
    for s in $SERVICES; do
        brew uninstall --force "rusty-photon-$s$SUFFIX" > /dev/null 2>&1 || true
    done
    brew untap "$TAP" > /dev/null 2>&1 || rm -rf "$TAP_DIR"
}
trap cleanup EXIT INT TERM

# A real tap, not a bare directory: tap-new lays out exactly the structure
# Homebrew expects, so install/untap behave deterministically (--no-git —
# nothing here outlives the run).
if [ ! -d "$TAP_DIR" ]; then
    brew tap-new --no-git "$TAP" > /dev/null
fi
mkdir -p "$TAP_DIR/Formula"
# Homebrew's tap-trust enforcement auto-trusts the formula named on the
# command line but refuses to load its DEPENDENCIES from an untrusted tap —
# which is exactly how the meta-formula pulls the family. Trust the scratch
# tap wholesale (skipped where the command doesn't exist yet).
if brew trust --help > /dev/null 2>&1; then
    brew trust "$TAP" > /dev/null
fi
# launchd only creates the log FILE; the service blocks' log_path parent must
# exist (a fresh runner's Homebrew prefix may not have var/log yet).
mkdir -p "$PREFIX/var/log"

if [ -n "$ONLY_SERVICES" ]; then
    scripts/generate-brew-formulas.sh --channel "$CHANNEL" --version "$VERSION" \
        --dist "$DIST_ABS" --url-base "file://$DIST_ABS" \
        --output "$TAP_DIR/Formula" --services "$ONLY_SERVICES"
else
    scripts/generate-brew-formulas.sh --channel "$CHANNEL" --version "$VERSION" \
        --dist "$DIST_ABS" --url-base "file://$DIST_ABS" \
        --output "$TAP_DIR/Formula"
fi

fail() {
    svc="$1"
    shift
    echo "verify-brew: FAIL [$svc]: $*" >&2
    echo "--- service log tail ($PREFIX/var/log/rusty-photon-$svc.log)" >&2
    tail -n 40 "$PREFIX/var/log/rusty-photon-$svc.log" >&2 2> /dev/null || true
    echo "--- brew services info" >&2
    svc_info=$(brew services info "rusty-photon-$svc$SUFFIX" --json 2> /dev/null || true)
    printf '%s\n' "$svc_info" >&2
    # A live-but-unresponsive process: sample where it is parked.
    svc_pid=$(printf '%s' "$svc_info" | sed -n 's/.*"pid": \([0-9]*\).*/\1/p' | head -1)
    if [ -n "$svc_pid" ] && command -v sample > /dev/null 2>&1; then
        echo "--- stack sample (pid $svc_pid)" >&2
        sample "$svc_pid" 1 2> /dev/null | sed -n '1,40p' >&2 || true
    fi
    # A direct foreground run surfaces what launchd cannot: dyld aborts print
    # to stderr, a segfault shows as a signal exit with no output at all, a
    # healthy start shows its startup lines.
    if [ -x "$PREFIX/bin/rusty-photon-$svc" ]; then
        echo "--- direct foreground run (8s)" >&2
        direct_log=$(mktemp)
        (
            "$PREFIX/bin/rusty-photon-$svc" > "$direct_log" 2>&1 &
            pid=$!
            sleep 8
            if kill -0 "$pid" 2> /dev/null; then
                kill "$pid" 2> /dev/null || true
                echo "direct run: still alive after 8s (killed)" >> "$direct_log"
            else
                wait "$pid" || echo "direct run: exited with status $?" >> "$direct_log"
            fi
        ) || true
        tail -n 40 "$direct_log" >&2 || true
        rm -f "$direct_log"
    fi
    echo "--- newest crash report (if any)" >&2
    # shellcheck disable=SC2012 # newest-first ordering is what ls -t is for
    newest=$(ls -t "$HOME/Library/Logs/DiagnosticReports/rusty-photon-$svc"* 2> /dev/null | head -1) || true
    if [ -n "${newest:-}" ]; then
        head -c 4000 "$newest" >&2 || true
        echo >&2
    fi
    exit 1
}

# ---- install ----------------------------------------------------------------
if [ -n "$ONLY_SERVICES" ]; then
    for s in $SERVICES; do
        echo "== $s: install"
        brew install "$TAP/rusty-photon-$s$SUFFIX" || fail "$s" "brew install failed"
    done
else
    # The meta-formula pulls the whole family — this IS the proof that the
    # channel's dependency wiring resolves.
    echo "== rusty-photon$SUFFIX: install (meta — pulls the whole family)"
    brew install "$TAP/rusty-photon$SUFFIX" || die "meta-formula install failed"
fi

# ---- per-service checks -----------------------------------------------------
for s in $SERVICES; do
    bin="$PREFIX/bin/rusty-photon-$s"
    [ -x "$bin" ] || fail "$s" "$bin missing after install"

    # The formula's own test block (--help probe).
    brew test "$TAP/rusty-photon-$s$SUFFIX" || fail "$s" "brew test failed"

    if [ "$s" = zwo-camera ]; then
        # ADR-014: exactly its own SDK dylib, resolved keg-relative.
        otool -L "$bin" | grep -q '@rpath/libASICamera2.dylib' \
            || fail "$s" "binary does not load @rpath/libASICamera2.dylib"
        if otool -L "$bin" | grep -qE 'libEFWFilter|libEAFFocuser'; then
            fail "$s" "binary links EFW/EAF SDKs it must not (ADR-014 per-device link)"
        fi
        [ -e "$PREFIX/opt/rusty-photon-zwo-camera$SUFFIX/lib/libASICamera2.dylib" ] \
            || fail "$s" "bundled libASICamera2.dylib missing from the keg"
    fi
    if [ "$s" = zwo-focuser ]; then
        otool -L "$bin" | grep -q '@rpath/libEAFFocuser.dylib' \
            || fail "$s" "binary does not load @rpath/libEAFFocuser.dylib"
        if otool -L "$bin" | grep -qE 'libASICamera2|libEFWFilter'; then
            fail "$s" "binary links ASI/EFW SDKs it must not (ADR-014 per-device link)"
        fi
        [ -e "$PREFIX/opt/rusty-photon-zwo-focuser$SUFFIX/lib/libEAFFocuser.dylib" ] \
            || fail "$s" "bundled libEAFFocuser.dylib missing from the keg"
    fi
    if [ "$s" = sentinel ]; then
        # Doctor rides in this formula (no rusty-photon-doctor formula):
        # the second binary must be installed and runnable, the renewal
        # plist rendered into the keg with this keg's paths, and the
        # timer's steady-state command (`tls renew`, nothing due) exit 0.
        doctor="$PREFIX/bin/rusty-photon-doctor"
        [ -x "$doctor" ] || fail "$s" "rusty-photon-doctor missing after install"
        code=0
        "$doctor" --json > /dev/null 2>&1 || code=$?
        [ "$code" -le 1 ] || fail "$s" "rusty-photon-doctor --json exited $code"
        "$doctor" tls renew > /dev/null 2>&1 \
            || fail "$s" "rusty-photon-doctor tls renew (nothing due) did not exit 0"
        plist="$PREFIX/opt/rusty-photon-sentinel$SUFFIX/rusty-photon-renew.plist"
        [ -f "$plist" ] || fail "$s" "rusty-photon-renew.plist missing from the keg"
        grep -q "rusty-photon-doctor" "$plist" \
            || fail "$s" "renewal plist does not run rusty-photon-doctor"
        plutil -lint "$plist" > /dev/null || fail "$s" "renewal plist does not lint"
    fi

    cfg="$CFG_DIR/$s.json"
    if is_gated "$s"; then
        # Never started: no config may appear, and the binary must at least
        # run (the brew test above proved --help).
        [ ! -e "$cfg" ] || fail "$s" "config exists for a gated service that never ran"
        echo "== $s: OK (installed; gated on config — not started)"
        continue
    fi

    echo "== $s: brew services start"
    rm -f "$PREFIX/var/log/rusty-photon-$s.log"
    brew services start "rusty-photon-$s$SUFFIX" || fail "$s" "brew services start failed"
    STARTED="$STARTED $s"

    if is_serial "$s"; then
        # Verifiable without hardware: the binary starts, self-creates its
        # config, and fails on the absent device — not earlier (loader or
        # config problems would die before the handshake).
        i=0
        until grep -q 'eager startup handshake' "$PREFIX/var/log/rusty-photon-$s.log" 2> /dev/null; do
            i=$((i + 1))
            [ "$i" -lt 30 ] || fail "$s" "no eager-handshake attempt in the service log"
            sleep 1
        done
        i=0
        until [ -s "$cfg" ]; do
            i=$((i + 1))
            [ "$i" -lt 15 ] || fail "$s" "config not self-created at $cfg"
            sleep 1
        done
        brew services stop "rusty-photon-$s$SUFFIX" > /dev/null || fail "$s" "brew services stop failed"
        echo "== $s: OK (config self-created; respawning on absent serial device)"
        continue
    fi

    port=$(port_of "$s")
    [ -n "$port" ] || fail "$s" "no port mapping — add $s to port_of()"
    path=$(probe_path "$s")

    if is_hid_tcc_gated "$s"; then
        # The launchd instance blocks in HID discovery without the privacy
        # grant; hold it to alive-not-crashlooping, record where it parks
        # (for the plan doc), then prove the binary serves in the foreground.
        sleep 3
        info=$(brew services info "rusty-photon-$s$SUFFIX" --json 2> /dev/null || true)
        printf '%s' "$info" | grep -q '"running": true' \
            || fail "$s" "launchd process not running (expected alive-but-blocked on the HID privacy grant)"
        hid_pid=$(printf '%s' "$info" | sed -n 's/.*"pid": \([0-9]*\).*/\1/p' | head -1)
        if [ -n "$hid_pid" ] && command -v sample > /dev/null 2>&1; then
            echo "-- $s: launchd instance blocked pre-bind; stack sample of pid $hid_pid:"
            sample "$hid_pid" 1 2> /dev/null | sed -n '1,40p' || true
        fi
        brew services stop "rusty-photon-$s$SUFFIX" > /dev/null || fail "$s" "brew services stop failed"
        "$PREFIX/bin/rusty-photon-$s" >> "$PREFIX/var/log/rusty-photon-$s.log" 2>&1 &
        fg_pid=$!
        i=0
        until curl -fsS -o /dev/null "http://127.0.0.1:$port$path" 2> /dev/null; do
            i=$((i + 1))
            if [ "$i" -ge 30 ]; then
                kill "$fg_pid" 2> /dev/null || true
                fail "$s" "no HTTP response on port $port ($path) even in a foreground run"
            fi
            sleep 1
        done
        kill "$fg_pid" 2> /dev/null || true
        wait "$fg_pid" 2> /dev/null || true
        echo "== $s: OK (foreground serve, port $port; launchd probe skipped — HID discovery needs a privacy grant, see docs/packaging-macos.md)"
        continue
    fi

    if [ "$s" = phd2-guider ]; then
        # No PHD2 on this machine: /health legitimately answers 503 (listener
        # up, guider not connected).
        i=0
        while :; do
            code=$(curl -sS -o /dev/null -w '%{http_code}' \
                "http://127.0.0.1:$port$path" 2> /dev/null || echo 000)
            [ "$code" = 200 ] || [ "$code" = 503 ] || {
                i=$((i + 1))
                [ "$i" -lt 30 ] || fail "$s" "no HTTP response on port $port ($path; last code $code)"
                sleep 1
                continue
            }
            break
        done
    else
        i=0
        until curl -fsS -o /dev/null "http://127.0.0.1:$port$path" 2> /dev/null; do
            i=$((i + 1))
            [ "$i" -lt 30 ] || fail "$s" "no HTTP response on port $port ($path)"
            sleep 1
        done
    fi

    if self_creates_config "$s"; then
        i=0
        until [ -s "$cfg" ]; do
            i=$((i + 1))
            [ "$i" -lt 15 ] || fail "$s" "config not self-created at $cfg"
            sleep 1
        done
    fi

    brew services stop "rusty-photon-$s$SUFFIX" > /dev/null || fail "$s" "brew services stop failed"
    echo "== $s: OK (service, port $port)"
done

# ---- uninstall lifecycle ----------------------------------------------------
# Uninstall the meta first (nothing depends on the services after that), then
# every service. Homebrew refuses to uninstall a dependency of an installed
# formula, so order matters.
if [ -z "$ONLY_SERVICES" ]; then
    brew uninstall "rusty-photon$SUFFIX" || die "meta-formula uninstall failed"
fi
for s in $SERVICES; do
    echo "== $s: uninstall"
    brew uninstall "rusty-photon-$s$SUFFIX" || fail "$s" "brew uninstall failed"
    [ ! -e "$PREFIX/bin/rusty-photon-$s" ] || fail "$s" "binary survived uninstall"
    if self_creates_config "$s"; then
        # Homebrew never purges: config survives uninstall (rpm-erase
        # parity); deleting ~/Library/Application Support/rusty-photon is
        # the documented manual step.
        [ -f "$CFG_DIR/$s.json" ] || fail "$s" "config did not survive uninstall (brew never purges)"
    fi
done

STARTED=""
echo ""
echo "verify-brew: OK ($(echo "$SERVICES" | wc -w | tr -d ' ') $CHANNEL formulas)"

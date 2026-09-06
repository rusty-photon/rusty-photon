#!/bin/sh
# verify-packages.sh — lifecycle-verify the built .deb packages in a podman
# systemd container (debian:trixie), or with --rpm the built .rpm packages
# in a Fedora one. Per service: install → unit active →
# config self-created at /var/lib/rusty-photon/.config/rusty-photon/<svc>.json
# → HTTP probe → remove (config survives) → purge (config + state gone; the
# shared user, home dir, and /etc/rusty-photon symlink stay). Class
# exceptions: ConditionPathExists-gated services (sky-survey-camera,
# plate-solver, calibrator-flats) verify enabled-but-inactive-and-not-failed;
# serial drivers verify config + handshake-attempted instead of active (see
# is_serial); the cameras, zwo-focuser, and phd2-guider never self-create a
# config (see self_creates_config); phd2-guider's /health legitimately
# answers 503 in the container (no PHD2), so its probe accepts 200 or 503.
# svbony-camera links an SDK its package may not ship (ADR-018), so the
# container walks the operator bootstrap — assert the blob is absent, run
# the packaged rusty-photon-svbony-sdk-install, then the ordinary contract
# (see needs_operator_sdk).
# The zwo services additionally prove via ldd that each binary resolves
# exactly its own bundled SDK blob through the RUNPATH (ADR-014), as does
# svbony-camera for its operator-installed one; sentinel additionally
# proves its polkit rule (the restart privilege path) is installed. Runs
# natively on arm64 (the rig) and x86_64 (dev box).
#
# The --rpm flavor differs where rpm's lifecycle genuinely differs:
# the scriptlets enable units without starting them (Fedora convention —
# asserted, then the script starts each unit itself), dnf must resolve the
# packages' declared requires from the repos (the dependency-adequacy
# proof: nothing is preinstalled to compensate), and erase behaves like
# remove, never purge — config and state must SURVIVE removal (manual
# cleanup is the documented story in docs/packaging.md). Everything after
# unit start — config, probes, class exceptions, the zwo ldd proof — is
# the same contract as the deb flavor.
#
# Rootless-container caveat (docs/plans/archive/service-packaging.md): rootless
# podman cannot apply the units' sandboxing across the User= switch
# (217/USER, 226/NAMESPACE), so this script pre-installs a drop-in that
# resets the whole hardening block inside the container. Containers verify
# the packaging lifecycle; the hardening is verified on real hosts
# (systemd-analyze security + an active unit on the rig).
#
# Usage: scripts/verify-packages.sh [--services a,b,c] [--dist DIR] [--rpm] [--keep]
#   --services a,b,c  verify only these (default: every rusty-photon-* package in DIST)
#   --dist DIR        package directory (default: dist/<workspace version>)
#   --rpm             verify the .rpm packages in a Fedora container instead
#                     of the .deb packages in a Debian one
#   --keep            keep the container on exit (debugging)

set -eu

die() { echo "verify-packages: $*" >&2; exit 1; }

usage() {
    sed -n '/^# Usage:/,/^$/{s/^# \{0,1\}//p}' "$0"
}

KEEP=0
DIST=""
ONLY_SERVICES=""
FLAVOR=deb
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
        --rpm) FLAVOR=rpm ;;
        --keep) KEEP=1 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown option: $1" ;;
    esac
    shift
done

[ -f packaging/postinst.common ] || die "run from the repo root"
command -v podman > /dev/null 2>&1 || die "podman not found"

if [ -z "$DIST" ]; then
    version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
    DIST="dist/$version"
fi
[ -d "$DIST" ] || die "$DIST not found — run scripts/build-packages.sh first"
DIST_ABS=$(cd "$DIST" && pwd)

# Service list: from the packages actually present, or --services. The deb
# name-version separator is `_`; rpm separates with `-`, which service names
# also contain, so the version is recognized by its leading digit (no
# service name has a `-<digit>` run — the checker-enforced names are the
# crate names).
if [ -n "$ONLY_SERVICES" ]; then
    SERVICES=$(echo "$ONLY_SERVICES" | tr ',' ' ')
elif [ "$FLAVOR" = rpm ]; then
    SERVICES=$(for f in "$DIST_ABS"/rusty-photon-*.rpm; do
        [ -e "$f" ] || continue
        b=$(basename "$f")
        b=${b#rusty-photon-}
        echo "$b" | sed 's/-[0-9].*//'
    done | sort -u | tr '\n' ' ')
else
    SERVICES=$(for f in "$DIST_ABS"/rusty-photon-*.deb; do
        [ -e "$f" ] || continue
        b=$(basename "$f")
        b=${b#rusty-photon-}
        echo "${b%%_*}"
    done | sort -u | tr '\n' ' ')
fi
[ -n "$SERVICES" ] || die "no rusty-photon-*.$FLAVOR packages in $DIST_ABS"
for s in $SERVICES; do
    # Exactly one package per service: a multi-match glob (e.g. two revisions
    # left over from an upgrade test) would make the in-container
    # install invocations below misbehave in confusing ways.
    if [ "$FLAVOR" = rpm ]; then
        set -- "$DIST_ABS/rusty-photon-${s}-"[0-9]*.rpm
        [ -e "$1" ] || die "no rusty-photon-${s}-*.rpm in $DIST_ABS"
        [ $# -eq 1 ] || die "multiple rusty-photon-${s}-*.rpm in $DIST_ABS — remove stale builds first"
    else
        set -- "$DIST_ABS/rusty-photon-${s}_"*.deb
        [ -e "$1" ] || die "no rusty-photon-${s}_*.deb in $DIST_ABS"
        [ $# -eq 1 ] || die "multiple rusty-photon-${s}_*.deb in $DIST_ABS — remove stale builds first"
    fi
done
echo "Verifying ($FLAVOR): $SERVICES"

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
        svbony-camera) echo 11125 ;;
        planetarium-bridge) echo 11126 ;;
        phd2-guider) echo 11130 ;;
        plate-solver) echo 11131 ;;
        calibrator-flats) echo 11170 ;;
        session-runner) echo 11171 ;;
        polar-align) echo 11172 ;;
        *) echo "" ;;
    esac
}

# Fail fast on missing port mappings, before the container build: checked
# only later, a gap would surface minutes into the lifecycle run, one
# service at a time. check-pkg-assets.sh cross-checks this table against
# verify-brew.sh's, but only when it is run.
missing=""
for s in $SERVICES; do
    [ -n "$(port_of "$s")" ] || missing="$missing $s"
done
[ -z "$missing" ] || die "no port mapping for:$missing — extend port_of()"

probe_path() {
    # Alpaca services answer the management API; the plain-HTTP services
    # (sentinel dashboard, rp orchestrator, ui-htmx BFF, phd2-guider,
    # session-runner, polar-align) expose /health.
    case "$1" in
        sentinel|rp|ui-htmx|phd2-guider|session-runner|polar-align) echo /health ;;
        *) echo /management/apiversions ;;
    esac
}

is_gated() {
    # No defaultable config → unit gated on ConditionPathExists (see plan).
    case "$1" in
        sky-survey-camera|plate-solver|calibrator-flats|session-runner|polar-align) return 0 ;;
        *) return 1 ;;
    esac
}

# The one SDK the packages never ship (ADR-018) and the helper that
# installs it — both flavors put them at the same paths.
SVBONY_SDK=/usr/lib/rusty-photon/libSVBCameraSDK.so
SVBONY_SDK_INSTALL=/usr/sbin/rusty-photon-svbony-sdk-install

needs_operator_sdk() {
    # svbony-camera links libSVBCameraSDK.so, which SVBony grants no license
    # to redistribute (ADR-018) — unlike zwo-camera, whose MIT blob rides in
    # the package, and unlike qhy-camera, which links its SDK statically and
    # defers only firmware. So this is the one package whose service cannot
    # start until the operator has run its download-on-target helper. The
    # container walks that same bootstrap.
    case "$1" in
        svbony-camera) return 0 ;;
        *) return 1 ;;
    esac
}

is_serial() {
    # Serial-device drivers validate hardware eagerly at startup and exit
    # when their device is absent (deliberate: fail startup instead of
    # advertising a broken device on the network); systemd then retries
    # every 5s until the device appears. The container has no serial
    # devices, so "active" is not the contract to verify for these.
    case "$1" in
        ppba-driver|qhy-focuser|pa-falcon-rotator|pa-scops-oag|dsd-fp2|star-adventurer-gti) return 0 ;;
        *) return 1 ;;
    esac
}

self_creates_config() {
    # The cameras and zwo-focuser deliberately do NOT materialize a config on
    # start (no materialize_identity — ASCOM UniqueIDs derive from SDK
    # serials; they run on defaults until config.apply / an operator writes a
    # file). phd2-guider likewise runs on built-in defaults and never writes
    # one. Gated services never start without one.
    if is_gated "$1"; then return 1; fi
    case "$1" in
        qhy-camera|zwo-camera|zwo-focuser|phd2-guider) return 1 ;;
        *) return 0 ;;
    esac
}

# ---- container ----------------------------------------------------------
IMG="localhost/rusty-photon-pkg-verify-$FLAVOR"
CNAME="rusty-photon-verify-$$"

echo "Building the verification image ($IMG)..."
if [ "$FLAVOR" = rpm ]; then
    # Deliberately NO runtime libraries here: dnf must pull every one of
    # them from the packages' declared requires, or the verification fails —
    # that is the dependency-adequacy proof. The installs are only what the
    # lifecycle machinery itself needs: systemd as init, udevadm for the
    # camera scriptlets, useradd/groupadd (shadow-utils) for the shared
    # user, curl for the probes, pgrep (procps-ng) for the stopped-on-erase
    # check.
    podman build -q -t "$IMG" - <<'EOF' > /dev/null
FROM registry.fedoraproject.org/fedora:44
RUN dnf -y install systemd systemd-udev shadow-utils procps-ng curl && dnf clean all
CMD ["/sbin/init"]
EOF
else
    podman build -q -t "$IMG" - <<'EOF' > /dev/null
FROM debian:trixie
# The stock Debian image ships a policy-rc.d that exits 101, silently
# blocking every unit start from maintainer scripts — exactly what this
# script exists to verify. Remove it; we run a real systemd via /sbin/init.
RUN apt-get update && apt-get install -y --no-install-recommends \
    systemd systemd-sysv udev ca-certificates curl \
    && rm -f /usr/sbin/policy-rc.d
CMD ["/sbin/init"]
EOF
fi

cleanup() {
    if [ "$KEEP" = 1 ]; then
        echo "Container kept: podman exec -it $CNAME bash (remove: podman rm -f $CNAME)"
    else
        podman rm -f "$CNAME" > /dev/null 2>&1 || true
    fi
}
trap cleanup EXIT INT TERM

podman run -d --name "$CNAME" --systemd=always \
    -v "$DIST_ABS:/dist:ro,Z" "$IMG" > /dev/null

cx() { podman exec "$CNAME" "$@"; }

fail() {
    svc="$1"
    shift
    echo "verify-packages: FAIL [$svc]: $*" >&2
    cx journalctl -u "rusty-photon-$svc" --no-pager -n 40 >&2 || true
    exit 1
}

echo "Waiting for systemd in the container..."
i=0
while :; do
    state=$(cx systemctl is-system-running 2> /dev/null || true)
    case "$state" in running | degraded) break ;; esac
    i=$((i + 1))
    [ "$i" -lt 60 ] || die "systemd did not come up in the container (state: $state)"
    sleep 1
done

# Hardening-reset drop-ins must exist BEFORE the debs install (postinst
# starts the units immediately).
for s in $SERVICES; do
    cx mkdir -p "/etc/systemd/system/rusty-photon-$s.service.d"
    podman exec -i "$CNAME" sh -c \
        "cat > /etc/systemd/system/rusty-photon-$s.service.d/99-container-verify.conf" <<'EOF'
# Rootless podman cannot set up the sandbox across the User= switch.
# Containers verify packaging lifecycle only; hardening is verified on hosts.
[Service]
NoNewPrivileges=no
ProtectSystem=off
ReadWritePaths=
ProtectHome=no
PrivateTmp=no
ProtectKernelTunables=no
ProtectKernelModules=no
ProtectControlGroups=no
RestrictSUIDSGID=no
LockPersonality=no
RestrictRealtime=no
RestrictAddressFamilies=
PrivateDevices=no
MemoryDenyWriteExecute=no
UMask=0022
EOF
done

if [ "$FLAVOR" = deb ]; then
    echo "Refreshing apt package lists in the container..."
    cx sh -c "apt-get update -qq"

    # Debs built on a non-Debian host carry an empty `$auto` Depends
    # (dpkg-shlibdeps needs Debian's shlibs database; cargo-deb warns and moves
    # on). Preinstall the known runtime libs so lifecycle verification still
    # works there; on Debian-host (rig) builds Depends is populated and apt
    # resolves it strictly, so this branch stays cold. The rpm flavor has no
    # equivalent: cargo-generate-rpm's builtin resolver works on any host, so
    # requires are always populated and always resolved strictly.
    compensate=0
    for s in $SERVICES; do
        dep=$(cx sh -c "dpkg-deb -f /dist/rusty-photon-${s}_*.deb Depends" 2> /dev/null || true)
        [ -n "$dep" ] || compensate=1
    done
    if [ "$compensate" = 1 ]; then
        echo "verify-packages: WARNING: deb(s) with empty Depends (non-Debian-host build); preinstalling runtime libs"
        cx sh -c "DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends libusb-1.0-0 libudev1 libstdc++6" > /dev/null
    fi
fi

# ---- install + per-service checks ----------------------------------------
for s in $SERVICES; do
    echo "== $s: install"
    if [ "$FLAVOR" = rpm ]; then
        # dnf resolves the rpm's declared requires from the Fedora repos —
        # a resolution failure here means a package under-declares its
        # runtime needs, which is exactly what this leg exists to catch.
        cx sh -c "dnf install -y /dist/rusty-photon-${s}-[0-9]*.rpm" \
            > /dev/null || fail "$s" "dnf install failed"
    else
        cx sh -c "DEBIAN_FRONTEND=noninteractive apt-get install -y /dist/rusty-photon-${s}_*.deb" \
            > /dev/null || fail "$s" "apt-get install failed"
    fi

    # Shared layout, created by the first daemon postinst and stable after.
    cx test -L /etc/rusty-photon || fail "$s" "/etc/rusty-photon symlink missing"
    cx sh -c "getent passwd rusty-photon > /dev/null" || fail "$s" "rusty-photon user missing"

    if [ "$FLAVOR" = rpm ]; then
        # The rpm scriptlets enable without starting (Fedora convention;
        # the deb postinst starts). Assert that exact contract, then start
        # the unit ourselves — from here the two flavors share the same
        # per-class expectations. `|| true`: gated units start-succeed with
        # an unmet condition, serial units die on the absent device; the
        # class branches below hold each to its own contract.
        cx systemctl is-enabled --quiet "rusty-photon-$s" \
            || fail "$s" "unit not enabled by the rpm scriptlet"
        if cx systemctl is-active --quiet "rusty-photon-$s"; then
            fail "$s" "unit auto-started on rpm install (scriptlets enable only)"
        fi
        cx systemctl start "rusty-photon-$s" 2> /dev/null || true
    fi

    cfg="/var/lib/rusty-photon/.config/rusty-photon/$s.json"
    if is_gated "$s"; then
        # No config → unit must be enabled but neither active nor failed.
        cx systemctl is-enabled --quiet "rusty-photon-$s" || fail "$s" "unit not enabled"
        if cx systemctl is-active --quiet "rusty-photon-$s"; then
            fail "$s" "gated unit is active without a config"
        fi
        if cx systemctl is-failed --quiet "rusty-photon-$s"; then
            fail "$s" "gated unit failed instead of waiting on ConditionPathExists"
        fi
        cx test ! -e "$cfg" || fail "$s" "config exists on a fresh install of a gated service"
        echo "== $s: OK (gated on config)"
        continue
    fi

    if needs_operator_sdk "$s"; then
        # First the ADR-018 guard: the package must have withheld the blob
        # (a bundled one would make every other assertion here vacuous).
        cx systemctl is-enabled --quiet "rusty-photon-$s" || fail "$s" "unit not enabled"
        cx test ! -e "$SVBONY_SDK" \
            || fail "$s" "package ships $SVBONY_SDK — ADR-018 forbids bundling it"
        # Then the documented bootstrap, run exactly as the postinst tells
        # an operator to — which also proves the helper's pinned sha256 and
        # per-arch blob names still resolve upstream. Deliberately no
        # `systemctl start` afterwards: until the blob lands the binary
        # cannot be loaded (exit 127) and the unit's Restart=on-failure is
        # what retries, exactly as the serial drivers retry an absent
        # device, so the ordinary active-within-30s check below is also the
        # proof that a bootstrapped host heals itself with no operator step.
        echo "== $s: installing the operator-supplied SDK ($SVBONY_SDK_INSTALL)"
        cx "$SVBONY_SDK_INSTALL" > /dev/null \
            || fail "$s" "$SVBONY_SDK_INSTALL failed"
        cx test -s "$SVBONY_SDK" || fail "$s" "helper did not install $SVBONY_SDK"
    fi

    if is_serial "$s"; then
        # Verifiable without hardware: the binary starts, self-creates its
        # config, and fails on the absent device — not earlier (loader,
        # user, or config problems would die before the handshake).
        i=0
        until cx sh -c "journalctl -u rusty-photon-$s --no-pager 2>/dev/null | grep -q 'eager startup handshake'"; do
            i=$((i + 1))
            [ "$i" -lt 30 ] || fail "$s" "no eager-handshake attempt in the journal"
            sleep 1
        done
        i=0
        until cx sh -c "test -s $cfg"; do
            i=$((i + 1))
            [ "$i" -lt 15 ] || fail "$s" "config not self-created at $cfg"
            sleep 1
        done
        if cx systemctl is-active --quiet "rusty-photon-$s"; then
            # In case a serial driver ever gains warn-and-serve, hold it to
            # the full contract.
            port=$(port_of "$s")
            [ -n "$port" ] || fail "$s" "no port mapping — add $s to port_of()"
            path=$(probe_path "$s")
            cx curl -fsS -o /dev/null "http://127.0.0.1:$port$path" 2> /dev/null \
                || fail "$s" "active but no HTTP response on port $port ($path)"
            echo "== $s: OK (active, config, port $port)"
        else
            echo "== $s: OK (config self-created; retrying on absent serial device)"
        fi
        continue
    fi

    i=0
    until cx systemctl is-active --quiet "rusty-photon-$s"; do
        i=$((i + 1))
        [ "$i" -lt 30 ] || fail "$s" "unit not active after 30s"
        sleep 1
    done

    if self_creates_config "$s"; then
        i=0
        until cx sh -c "test -s $cfg"; do
            i=$((i + 1))
            [ "$i" -lt 15 ] || fail "$s" "config not self-created at $cfg"
            sleep 1
        done
    fi

    port=$(port_of "$s")
    [ -n "$port" ] || fail "$s" "no port mapping — add $s to port_of()"
    path=$(probe_path "$s")
    if [ "$s" = phd2-guider ]; then
        # No PHD2 in the container: /health legitimately answers 503
        # (unit active, listener up, guider not connected). Verify the
        # listener answers 200 or 503 instead of requiring success.
        i=0
        while :; do
            code=$(cx curl -sS -o /dev/null -w '%{http_code}' \
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
        until cx curl -fsS -o /dev/null "http://127.0.0.1:$port$path" 2> /dev/null; do
            i=$((i + 1))
            [ "$i" -lt 30 ] || fail "$s" "no HTTP response on port $port ($path)"
            sleep 1
        done
    fi

    if [ "$s" = sentinel ]; then
        # The restart privilege path: without this polkit rule the
        # NoNewPrivileges unit could restart nothing via systemctl (the
        # grant itself is verified on a real host — the container has no
        # polkitd; here we prove the package puts the rule where the
        # JS-rules polkitd reads vendor rules).
        cx test -f /usr/share/polkit-1/rules.d/50-rusty-photon-sentinel.rules \
            || fail "$s" "polkit rule not installed at /usr/share/polkit-1/rules.d"
        # Doctor rides in this package (no rusty-photon-doctor package
        # exists): the binary must be installed and runnable, and the
        # renewal timer armed. A diagnosis may legitimately find problems
        # in the container (exit 1); exit 2 would mean the binary crashed.
        cx test -x /usr/bin/rusty-photon-doctor \
            || fail "$s" "rusty-photon-doctor not installed at /usr/bin"
        code=0
        cx rusty-photon-doctor --json > /dev/null 2>&1 || code=$?
        [ "$code" -le 1 ] || fail "$s" "rusty-photon-doctor --json exited $code"
        # `tls renew` with nothing staged is the timer's steady state and
        # must exit 0 — this is exactly what the daily unit will run.
        cx rusty-photon-doctor tls renew > /dev/null 2>&1 \
            || fail "$s" "rusty-photon-doctor tls renew (nothing due) did not exit 0"
        cx systemctl is-enabled --quiet rusty-photon-renew.timer \
            || fail "$s" "rusty-photon-renew.timer not enabled"
        if [ "$FLAVOR" != rpm ]; then
            # deb postinst starts the timer; rpm (Fedora convention)
            # enables only — it arms on the next boot.
            cx systemctl is-active --quiet rusty-photon-renew.timer \
                || fail "$s" "rusty-photon-renew.timer not active (postinst starts it)"
        fi
    fi
    if [ "$s" = zwo-camera ]; then
        # RUNPATH proof: the SONAME-less bundled blob resolves from the shared
        # /usr/lib/rusty-photon dir — and ONLY the camera SDK is needed
        # (ADR-014: per-device link features; the EFW/EAF SDKs belong to other
        # services and are not shipped by this package).
        cx sh -c "ldd /usr/bin/rusty-photon-zwo-camera | grep -q /usr/lib/rusty-photon/libASICamera2.so" \
            || fail "$s" "libASICamera2.so does not resolve to /usr/lib/rusty-photon (RUNPATH)"
        if cx sh -c "ldd /usr/bin/rusty-photon-zwo-camera | grep -qE 'libEFWFilter|libEAFFocuser'"; then
            fail "$s" "binary links EFW/EAF SDKs it must not (ADR-014 per-device link)"
        fi
    fi
    if [ "$s" = svbony-camera ]; then
        # Same RUNPATH proof the zwo services get, for the blob the operator
        # installed a moment ago: the package's own binary must resolve it
        # from /usr/lib/rusty-photon with no ldconfig step anywhere.
        cx sh -c "ldd /usr/bin/rusty-photon-svbony-camera | grep -q /usr/lib/rusty-photon/libSVBCameraSDK.so" \
            || fail "$s" "libSVBCameraSDK.so does not resolve to /usr/lib/rusty-photon (RUNPATH)"
    fi
    if [ "$s" = zwo-focuser ]; then
        # Same proof for the focuser: exactly its own blob, nothing else.
        cx sh -c "ldd /usr/bin/rusty-photon-zwo-focuser | grep -q /usr/lib/rusty-photon/libEAFFocuser.so" \
            || fail "$s" "libEAFFocuser.so does not resolve to /usr/lib/rusty-photon (RUNPATH)"
        if cx sh -c "ldd /usr/bin/rusty-photon-zwo-focuser | grep -qE 'libASICamera2|libEFWFilter'"; then
            fail "$s" "binary links ASI/EFW SDKs it must not (ADR-014 per-device link)"
        fi
    fi
    echo "== $s: OK (active, config, port $port)"
done

# ---- remove / purge lifecycle ---------------------------------------------
for s in $SERVICES; do
    cfg="/var/lib/rusty-photon/.config/rusty-photon/$s.json"
    if [ "$FLAVOR" = rpm ]; then
        # rpm has no purge lifecycle: erase behaves like dpkg remove. The
        # payload and unit go, the process is stopped (pre_uninstall), and
        # the runtime-created config + state SURVIVE — their deletion is
        # the documented manual step in docs/packaging.md. Not every
        # service materializes a state dir, so its survival is asserted
        # only when it existed before the erase.
        echo "== $s: erase"
        state_existed=0
        if cx test -d "/var/lib/rusty-photon/$s"; then
            state_existed=1
        fi
        cx sh -c "dnf remove -y rusty-photon-$s" > /dev/null \
            || fail "$s" "dnf remove failed"
        cx test ! -e "/usr/bin/rusty-photon-$s" || fail "$s" "binary survived erase"
        cx test ! -e "/usr/lib/systemd/system/rusty-photon-$s.service" \
            || fail "$s" "unit file survived erase"
        if cx pgrep -f "/usr/bin/rusty-photon-$s" > /dev/null 2>&1; then
            fail "$s" "process still running after erase (pre_uninstall must stop it)"
        fi
        if self_creates_config "$s"; then
            cx test -f "$cfg" || fail "$s" "config did not survive erase (rpm never purges)"
        fi
        if [ "$state_existed" = 1 ]; then
            cx test -d "/var/lib/rusty-photon/$s" \
                || fail "$s" "state dir did not survive erase (rpm never purges)"
        fi
    else
        echo "== $s: remove + purge"
        cx sh -c "DEBIAN_FRONTEND=noninteractive apt-get remove -y rusty-photon-$s" > /dev/null \
            || fail "$s" "apt-get remove failed"
        if self_creates_config "$s"; then
            cx test -f "$cfg" || fail "$s" "config did not survive remove (must only go on purge)"
        fi
        cx dpkg --purge "rusty-photon-$s" > /dev/null || fail "$s" "dpkg --purge failed"
        cx test ! -e "$cfg" || fail "$s" "config survived purge"
        cx test ! -e "/var/lib/rusty-photon/$s" || fail "$s" "state dir survived purge"
    fi
done

# The shared pieces are never removed — shared across packages, so no
# scriptlet of either flavor touches them (Debian convention for system users).
cx sh -c "getent passwd rusty-photon > /dev/null" || die "shared user removed by uninstall"
cx test -d /var/lib/rusty-photon || die "shared home removed by uninstall"
cx test -L /etc/rusty-photon || die "/etc/rusty-photon symlink removed by uninstall"

echo ""
echo "verify-packages: OK ($(echo "$SERVICES" | wc -w | tr -d ' ') $FLAVOR packages)"

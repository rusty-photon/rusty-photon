#!/bin/sh
# Asserts the per-service packaging invariants documented in
# packaging/README.md and docs/plans/archive/service-packaging.md.
# Run from the repo root; exits non-zero on any violation.
set -eu

fail=0
err() { echo "check-pkg-assets: $*" >&2; fail=1; }

# Print the body of one TOML table (from its [header] to the next [header]),
# so key checks are position-independent within the section. Array-of-tables
# entries ([[header]], sentinel's two systemd-units) print as one body.
toml_section() { # $1=file $2=exact table name
    # Only bare [header] / [[header]] lines bound a section: scriptlet
    # bodies inside multi-line strings legitimately start lines with
    # "[ -e ..." tests, which are not headers.
    awk -v w1="[$2]" -v w2="[[$2]]" '
        /^\[\[?[A-Za-z0-9_.-]+\]\]?$/ { in_section = ($0 == w1 || $0 == w2); next }
        in_section { print }
    ' "$1"
}

[ -f packaging/postinst.common ] || { echo "check-pkg-assets: run from the repo root" >&2; exit 2; }

# The build script carries the SDK pins and the RUNPATH every package's
# binaries are linked with; several checks below cross-reference it.
bp=scripts/build-packages.sh

for pkgdir in services/*/pkg; do
    [ -d "$pkgdir" ] || continue
    svc=$(basename "$(dirname "$pkgdir")")
    name="rusty-photon-$svc"
    unit="$pkgdir/$name.service"
    toml="services/$svc/Cargo.toml"

    # Serial-class services keep per-flavor unit copies: Debian's unit
    # confers plugdev on top of dialout (openocd-class udev rules there put
    # FTDI serial nodes in plugdev, and base-passwd guarantees the group
    # exists), while the rpm unit stays dialout-only — plugdev is a Debian
    # group that must never be created on rpm-family hosts.
    case "$svc" in
        ppba-driver|upbv2-driver|qhy-focuser|pa-falcon-rotator|pa-scops-oag|dsd-fp2|star-adventurer-gti)
            units="$pkgdir/deb/$name.service $pkgdir/rpm/$name.service" ;;
        *)
            units="$pkgdir/$name.service" ;;
    esac
    for unit in $units; do
        if [ ! -f "$unit" ]; then
            err "$svc: missing $unit"
            continue
        fi
        grep -q "^ExecStart=/usr/bin/$name\$" "$unit" \
            || err "$svc: ExecStart must be exactly /usr/bin/$name (config is XDG-resolved; no --config flag)"
        # Reload-capable services (ServiceRunner::with_reload) expose SIGHUP.
        case "$svc" in
            filemonitor|ppba-driver|upbv2-driver|qhy-focuser|sky-survey-camera|pa-falcon-rotator|pa-scops-oag|dsd-fp2|star-adventurer-gti|qhy-camera|zwo-camera|zwo-focuser|svbony-camera)
                grep -q '^ExecReload=/bin/kill -HUP \$MAINPID$' "$unit" \
                    || err "$svc: reload-capable service must have ExecReload=/bin/kill -HUP \$MAINPID"
                ;;
        esac
        # Services with no defaultable config gate on the config file existing
        # instead of crash-looping on a fresh install.
        case "$svc" in
            sky-survey-camera|plate-solver|calibrator-flats|session-runner|polar-align)
                grep -q "^ConditionPathExists=/var/lib/rusty-photon/\.config/rusty-photon/$svc\.json\$" "$unit" \
                    || err "$svc: no-default-config service must gate on ConditionPathExists=<XDG config path>"
                ;;
        esac
    done
    case "$svc" in
        ppba-driver|upbv2-driver|qhy-focuser|pa-falcon-rotator|pa-scops-oag|dsd-fp2|star-adventurer-gti)
            deb_unit="$pkgdir/deb/$name.service"
            rpm_unit="$pkgdir/rpm/$name.service"
            if [ -f "$deb_unit" ] && [ -f "$rpm_unit" ]; then
                grep -q '^SupplementaryGroups=dialout plugdev$' "$deb_unit" \
                    || err "$svc: deb unit must carry SupplementaryGroups=dialout plugdev"
                grep -q '^SupplementaryGroups=dialout$' "$rpm_unit" \
                    || err "$svc: rpm unit must carry exactly SupplementaryGroups=dialout"
                stripped_deb=$(mktemp); stripped_rpm=$(mktemp)
                grep -v '^SupplementaryGroups=' "$deb_unit" > "$stripped_deb"
                grep -v '^SupplementaryGroups=' "$rpm_unit" > "$stripped_rpm"
                cmp -s "$stripped_deb" "$stripped_rpm" \
                    || err "$svc: deb and rpm units must differ only in the SupplementaryGroups line"
                rm -f "$stripped_deb" "$stripped_rpm"
            fi
            toml_section "$toml" "package.metadata.deb.systemd-units" \
                | grep -q '^unit-scripts = "pkg/deb/"$' \
                || err "$svc: deb systemd-units must take the unit from pkg/deb/"
            toml_section "$toml" "package.metadata.generate-rpm" \
                | grep -q "source = \"pkg/rpm/$name.service\"" \
                || err "$svc: rpm assets must ship the pkg/rpm/ unit"
            ;;
        # Camera-class packages grant device access via the service
        # account's own group: the udev rule assigns GROUP="rusty-photon"
        # and the unit needs no SupplementaryGroups at all.
        qhy-camera|zwo-camera|zwo-focuser|svbony-camera)
            for rule in "$pkgdir"/90-*.rules; do
                [ -f "$rule" ] || continue
                grep -q 'GROUP="rusty-photon"' "$rule" \
                    || err "$svc: $(basename "$rule") must assign GROUP=\"rusty-photon\""
                grep -q 'GROUP="plugdev"' "$rule" \
                    && err "$svc: $(basename "$rule") must not assign plugdev"
            done
            grep -q '^SupplementaryGroups=' "$pkgdir/$name.service" \
                && err "$svc: camera unit needs no SupplementaryGroups (nodes are rusty-photon-owned)"
            ;;
    esac

    # postrm is byte-identical everywhere. postinst is byte-identical too,
    # except the udev-shipping packages: theirs must equal postinst.common
    # with the canonical udev stanza inserted before #DEBHELPER# (still
    # deterministic). zwo-focuser reuses the same camera-class stanza even
    # though its firmware lives in onboard flash (like the ASI camera and
    # EFW) — the stanza's fw_helper check is a harmless no-op for it (the
    # package name doesn't end in "-camera", so the helper path can never
    # exist), and a byte-identical shared file beats a third variant.
    cmp -s "packaging/postrm.common" "$pkgdir/postrm" \
        || err "$svc: pkg/postrm differs from packaging/postrm.common"
    case "$svc" in
        qhy-camera|zwo-camera|zwo-focuser)
            expected=$(mktemp)
            awk '/^#DEBHELPER#/ { while ((getline line < "packaging/postinst.udev-stanza") > 0) print line } { print }' \
                packaging/postinst.common > "$expected"
            cmp -s "$expected" "$pkgdir/postinst" \
                || err "$svc: pkg/postinst must be postinst.common + udev stanza before #DEBHELPER#"
            rm -f "$expected"
            ;;
        # svbony-camera cannot reuse the shared postinst.udev-stanza
        # byte-for-byte: that stanza derives the SDK helper's name from
        # $DPKG_MAINTSCRIPT_PACKAGE (stripping "-camera" and appending
        # "-firmware-install"), but per ADR-018 SVBony's helper is named
        # rusty-photon-svbony-sdk-install, not
        # rusty-photon-svbony-firmware-install — so its postinst spells the
        # pointer out directly instead. Assert the load-bearing pieces
        # (still postinst.common's user/dir/symlink preamble, an
        # unconditional udev reload, and a pointer at the exact helper
        # path) rather than requiring byte identity to a template it
        # deliberately does not use.
        svbony-camera)
            # postinst.common's own last line is #DEBHELPER#, which this
            # package's postinst must NOT have yet at that offset (more
            # content is inserted before it) — compare everything except
            # that trailing line instead.
            preamble_lines=$(($(wc -l < "packaging/postinst.common") - 1))
            common_body=$(mktemp); actual_body=$(mktemp)
            head -n "$preamble_lines" "packaging/postinst.common" > "$common_body"
            head -n "$preamble_lines" "$pkgdir/postinst" > "$actual_body"
            cmp -s "$common_body" "$actual_body" \
                || err "$svc: pkg/postinst's user/dir/symlink preamble must match packaging/postinst.common"
            rm -f "$common_body" "$actual_body"
            grep -q '^udevadm control --reload-rules || true$' "$pkgdir/postinst" \
                || err "$svc: pkg/postinst must reload udev rules"
            grep -q '^udevadm trigger || true$' "$pkgdir/postinst" \
                || err "$svc: pkg/postinst must trigger udev"
            grep -q "rusty-photon-svbony-sdk-install' once as root" "$pkgdir/postinst" \
                || err "$svc: pkg/postinst must point the operator at rusty-photon-svbony-sdk-install"
            grep -q '^#DEBHELPER#$' "$pkgdir/postinst" \
                || err "$svc: pkg/postinst must end with #DEBHELPER#"
            ;;
        *)
            cmp -s "packaging/postinst.common" "$pkgdir/postinst" \
                || err "$svc: pkg/postinst differs from packaging/postinst.common"
            ;;
    esac

    # Camera packages ship their own udev rule (the postinst udev stanza
    # reloads rules on install, so the rule file must actually be there).
    # Sentinel ships the polkit rule that authorizes its restart commands
    # (no postinst stanza needed — polkitd hot-reloads rules.d).
    case "$svc" in
        sentinel)
            [ -f "$pkgdir/50-rusty-photon-sentinel.rules" ] \
                || err "$svc: missing pkg/50-rusty-photon-sentinel.rules"
            # Sentinel's package also carries the doctor binary and the TLS
            # renewal units (no rusty-photon-doctor package exists; plan
            # decision 8) — assert the whole delivery contract so drift in
            # any one of the pieces is caught here, not on a rig.
            renew_service="$pkgdir/rusty-photon-renew.service"
            renew_timer="$pkgdir/rusty-photon-renew.timer"
            if [ ! -f "$renew_service" ]; then
                err "$svc: missing $renew_service"
            else
                grep -q '^Type=oneshot$' "$renew_service" \
                    || err "$svc: rusty-photon-renew.service must be Type=oneshot"
                grep -q '^ExecStart=/usr/bin/rusty-photon-doctor tls renew$' "$renew_service" \
                    || err "$svc: rusty-photon-renew.service must ExecStart rusty-photon-doctor tls renew"
                grep -q '^User=rusty-photon$' "$renew_service" \
                    || err "$svc: rusty-photon-renew.service must run as the service user"
                grep -q '^\[Install\]' "$renew_service" \
                    && err "$svc: rusty-photon-renew.service must stay static (no [Install]; the timer arms it)"
            fi
            if [ ! -f "$renew_timer" ]; then
                err "$svc: missing $renew_timer"
            else
                grep -q '^OnCalendar=daily$' "$renew_timer" \
                    || err "$svc: rusty-photon-renew.timer must fire OnCalendar=daily"
                grep -q '^Persistent=true$' "$renew_timer" \
                    || err "$svc: rusty-photon-renew.timer must be Persistent (catch up after power-off)"
                grep -q '^WantedBy=timers.target$' "$renew_timer" \
                    || err "$svc: rusty-photon-renew.timer must be WantedBy=timers.target"
            fi
            toml_section "$toml" "package.metadata.deb" \
                | grep -q '"target/release/doctor", "usr/bin/rusty-photon-doctor", "755"' \
                || err "$svc: deb assets must ship target/release/doctor as usr/bin/rusty-photon-doctor"
            toml_section "$toml" "package.metadata.deb.systemd-units" \
                | grep -q '^unit-name = "rusty-photon-renew"$' \
                || err "$svc: deb systemd-units must carry a rusty-photon-renew entry (enables the timer)"
            rpm_section=$(toml_section "$toml" "package.metadata.generate-rpm")
            echo "$rpm_section" | grep -q 'dest = "/usr/bin/rusty-photon-doctor"' \
                || err "$svc: rpm assets must ship the doctor binary"
            echo "$rpm_section" | grep -q 'dest = "/usr/lib/systemd/system/rusty-photon-renew.service"' \
                || err "$svc: rpm assets must ship rusty-photon-renew.service"
            echo "$rpm_section" | grep -q 'dest = "/usr/lib/systemd/system/rusty-photon-renew.timer"' \
                || err "$svc: rpm assets must ship rusty-photon-renew.timer"
            echo "$rpm_section" | grep -q 'systemctl enable rusty-photon-renew.timer' \
                || err "$svc: rpm post_install must enable rusty-photon-renew.timer"
            ;;
        qhy-camera)
            [ -f "$pkgdir/90-rusty-photon-qhy.rules" ] \
                || err "$svc: missing pkg/90-rusty-photon-qhy.rules"
            ;;
        zwo-camera)
            [ -f "$pkgdir/90-rusty-photon-zwo.rules" ] \
                || err "$svc: missing pkg/90-rusty-photon-zwo.rules"
            ;;
        zwo-focuser)
            # Uniquely named (not zwo-camera's 90-rusty-photon-zwo.rules) so
            # both packages can install their udev rule on the same host
            # without a dpkg/rpm filename collision.
            [ -f "$pkgdir/90-rusty-photon-zwo-focuser.rules" ] \
                || err "$svc: missing pkg/90-rusty-photon-zwo-focuser.rules"
            ;;
        svbony-camera)
            [ -f "$pkgdir/90-rusty-photon-svbony.rules" ] \
                || err "$svc: missing pkg/90-rusty-photon-svbony.rules"
            helper="$pkgdir/rusty-photon-svbony-sdk-install"
            if [ ! -f "$helper" ]; then
                err "$svc: missing pkg/rusty-photon-svbony-sdk-install"
            else
                # The helper's install directory must be the RUNPATH
                # build-packages.sh bakes into the binary: the operator
                # installs the blob at runtime, and nothing else makes the
                # loader look there (that directory is deliberately off
                # ldconfig's scan path — ADR-013/ADR-018).
                rpath=$(sed -n 's/.*-Wl,-rpath,\([^"]*\)".*/\1/p' "$bp" | head -1)
                grep -q '^DEST="\$DEST_DIR/libSVBCameraSDK\.so"$' "$helper" \
                    && grep -q "^DEST_DIR=\"\\\$ROOT$rpath\"\$" "$helper" \
                    || err "$svc: helper must install libSVBCameraSDK.so into build-packages.sh's RUNPATH ($rpath)"
                # Unpackaged-host device access (issue #710). The rule the
                # helper writes must carry the same USB VID as the packaged
                # one, must NOT reuse its filename (an /etc file of the same
                # name masks the packaged rule, silently keeping dev
                # ownership after the package lands), and must never fall
                # back to a world-writable node (ADR-013 §3).
                vid=$(sed -n 's/.*ATTRS{idVendor}=="\([0-9a-f]*\)".*/\1/p' \
                    "$pkgdir/90-rusty-photon-svbony.rules" | head -1)
                grep -q "ATTRS{idVendor}==\"$vid\"" "$helper" \
                    || err "$svc: helper's udev rule must match the packaged rule's VID ($vid)"
                grep -q '^DEV_RULE=90-rusty-photon-svbony-dev\.rules$' "$helper" \
                    || err "$svc: helper's dev udev rule must not reuse 90-rusty-photon-svbony.rules (it would mask the packaged rule)"
                grep -q 'MODE="0666"' "$helper" \
                    && err "$svc: helper must not install a world-writable udev rule (ADR-013 §3)"
            fi
            toml_section "$toml" "package.metadata.deb" \
                | grep -q '"pkg/rusty-photon-svbony-sdk-install", "usr/sbin/rusty-photon-svbony-sdk-install", "755"' \
                || err "$svc: deb assets must ship rusty-photon-svbony-sdk-install"
            toml_section "$toml" "package.metadata.generate-rpm" \
                | grep -q 'dest = "/usr/sbin/rusty-photon-svbony-sdk-install"' \
                || err "$svc: rpm assets must ship rusty-photon-svbony-sdk-install"
            ;;
    esac

    toml_section "$toml" "package.metadata.deb" | grep -q "^name = \"$name\"" \
        || err "$svc: [package.metadata.deb] name must be \"$name\""
    toml_section "$toml" "package.metadata.deb.systemd-units" | grep -q "^unit-name = \"$name\"" \
        || err "$svc: [package.metadata.deb.systemd-units] unit-name must be \"$name\""
    toml_section "$toml" "package.metadata.generate-rpm" | grep -q "^name = \"$name\"" \
        || err "$svc: [package.metadata.generate-rpm] name must be \"$name\""
done

# The QHY SDK pins (version + archive sha256s) live in two shipped places
# (ADR-013); they must match.
fw=services/qhy-camera/pkg/rusty-photon-qhy-firmware-install
if [ -f "$bp" ] && [ -f "$fw" ]; then
    v1=$(sed -n 's/^QHY_SDK_VERSION="\(.*\)"$/\1/p' "$bp" | head -1)
    v2=$(sed -n 's/^VERSION="\(.*\)"$/\1/p' "$fw" | head -1)
    if [ -z "$v1" ] || [ "$v1" != "$v2" ]; then
        err "QHY SDK version pin mismatch: build-packages.sh='$v1' vs firmware-install='$v2'"
    fi
    for arch in X86_64 AARCH64; do
        s1=$(sed -n "s/^QHY_SHA256_$arch=\"\(.*\)\"\$/\1/p" "$bp" | head -1)
        s2=$(sed -n "s/^SHA256_$arch=\"\(.*\)\"\$/\1/p" "$fw" | head -1)
        if [ -z "$s1" ] || [ "$s1" != "$s2" ]; then
            err "QHY SDK sha256 pin mismatch ($arch): build-packages.sh='$s1' vs firmware-install='$s2'"
        fi
    done
fi

# The packaged ZWO SDK license must match the copy vendored with the SDK
# headers (single upstream source; the pkg/ copy exists only because cargo-deb
# assets should stay inside the crate directory).
zv=crates/zwo-rs/libzwo-sys/sdk/include/license.txt
for svc in zwo-camera zwo-focuser; do
    zl="services/$svc/pkg/ZWO-SDK-LICENSE.txt"
    cmp -s "$zv" "$zl" \
        || err "$svc: pkg/ZWO-SDK-LICENSE.txt differs from $zv"
done

# The ZWO blob ref is pinned in two places once the build script exists:
# build-packages.sh (stages pkg/lib/ for the deb) and the CI action's default
# ref (provisions the link-time SDK) — the shipped blobs and the CI-linked
# blobs must come from the same indi-3rdparty commit.
act=.github/actions/install-zwo-sdk/action.yml
if [ -f "$bp" ] && [ -f "$act" ]; then
    z1=$(sed -n 's/^ZWO_SDK_REF="\(.*\)"$/\1/p' "$bp" | head -1)
    z2=$(awk '$1 == "ref:" { in_ref = 1 } in_ref && $1 == "default:" { print $2; exit }' "$act")
    if [ -z "$z1" ] || [ "$z1" != "$z2" ]; then
        err "ZWO SDK ref pin mismatch: build-packages.sh='$z1' vs install-zwo-sdk default='$z2'"
    fi
fi

# The SVBony blob ref is pinned in three places: the blob build-packages.sh
# links against at build time, the blob CI links against (install-svbony-sdk),
# and the blob the operator-run helper downloads post-install
# (rusty-photon-svbony-sdk-install) — all three must come from the same
# indi-3rdparty commit.
svbony_helper=services/svbony-camera/pkg/rusty-photon-svbony-sdk-install
svbony_act=.github/actions/install-svbony-sdk/action.yml
if [ -f "$svbony_helper" ] && [ -f "$svbony_act" ]; then
    s1=$(sed -n 's/^REF="\(.*\)"$/\1/p' "$svbony_helper" | head -1)
    s2=$(awk '$1 == "ref:" { in_ref = 1 } in_ref && $1 == "default:" { print $2; exit }' "$svbony_act")
    if [ -z "$s1" ] || [ "$s1" != "$s2" ]; then
        err "SVBony SDK ref pin mismatch: rusty-photon-svbony-sdk-install='$s1' vs install-svbony-sdk default='$s2'"
    fi
fi
if [ -f "$bp" ] && [ -f "$svbony_act" ]; then
    s3=$(sed -n 's/^SVBONY_SDK_REF="\(.*\)"$/\1/p' "$bp" | head -1)
    s4=$(awk '$1 == "ref:" { in_ref = 1 } in_ref && $1 == "default:" { print $2; exit }' "$svbony_act")
    if [ -z "$s3" ] || [ "$s3" != "$s4" ]; then
        err "SVBony SDK ref pin mismatch: build-packages.sh='$s3' vs install-svbony-sdk default='$s4'"
    fi
fi

# plugdev is Debian's group (base-passwd ships it); nothing we ship may
# create it on rpm-family hosts, and no rpm-side unit or rule may depend
# on it.
if grep -l 'groupadd -r plugdev' services/*/Cargo.toml > /dev/null 2>&1; then
    err "rpm scriptlets must never create plugdev (Debian-only group): $(grep -l 'groupadd -r plugdev' services/*/Cargo.toml | tr '\n' ' ')"
fi
for rpm_unit in services/*/pkg/rpm/*.service; do
    [ -f "$rpm_unit" ] || continue
    grep -q 'plugdev' "$rpm_unit" \
        && err "$rpm_unit: rpm units must not reference plugdev"
done

# ---- macOS tarballs (scripts/build-tarballs.sh, nightly-releases.md N4) -----
# The mac build script carries its own copies of the SDK pins (its sha256
# pins the mac arm64 archives specifically, so only the versions/refs are
# shared): the QHY SDK version must match build-packages.sh (same release
# linked on every OS) and the ZWO blob ref must match the CI action (shipped
# dylibs and CI-linked dylibs from the same indi-3rdparty commit).
bt=scripts/build-tarballs.sh
if [ -f "$bp" ] && [ -f "$bt" ]; then
    v1=$(sed -n 's/^QHY_SDK_VERSION="\(.*\)"$/\1/p' "$bp" | head -1)
    vm=$(sed -n 's/^QHY_SDK_VERSION="\(.*\)"$/\1/p' "$bt" | head -1)
    if [ -z "$vm" ] || [ "$v1" != "$vm" ]; then
        err "QHY SDK version pin mismatch: build-packages.sh='$v1' vs build-tarballs.sh='$vm'"
    fi
fi
if [ -f "$bt" ] && [ -f "$act" ]; then
    z1=$(sed -n 's/^ZWO_SDK_REF="\(.*\)"$/\1/p' "$bt" | head -1)
    z2=$(awk '$1 == "ref:" { in_ref = 1 } in_ref && $1 == "default:" { print $2; exit }' "$act")
    if [ -z "$z1" ] || [ "$z1" != "$z2" ]; then
        err "ZWO SDK ref pin mismatch: build-tarballs.sh='$z1' vs install-zwo-sdk default='$z2'"
    fi
fi

# svbony-camera has no confirmed mac_arm64 SVBony SDK blob (see
# build-tarballs.sh's own exclusion comment), so it must never appear in a
# macOS tarball or Homebrew formula: both build-tarballs.sh and
# generate-brew-formulas.sh must carry the matching exclusion guard, so a
# formula never gets rendered pointing at a tarball that was never built.
gbf=scripts/generate-brew-formulas.sh
if [ -f "$bt" ] && ! grep -qF '*" svbony-camera "*)' "$bt"; then
    err "$bt: svbony-camera exclusion guard missing (no confirmed mac_arm64 SVBony SDK blob)"
fi
if [ -f "$gbf" ] && ! grep -qF '*" svbony-camera "*)' "$gbf"; then
    err "$gbf: svbony-camera exclusion guard missing — must match build-tarballs.sh's, or formula generation will fail on the missing tarball"
fi

# ---- Windows suite MSI (installer/, ADR-015) --------------------------------
# The Windows package set is the Linux one (services/*/pkg); session-runner
# now ships on all platforms (its Linux .deb closed the follow-up that
# docs/plans/archive/windows-packaging.md tracked). svbony-camera is excluded: it
# has no Windows SVBony SDK at all (docs/services/svbony-camera.md's Phase F
# notes — excluded entirely from the Windows per-service matrix), unlike
# every other packaged service, so there is no Windows package for
# `win_port_of()`/`installer/fragments/` to describe.
WIN_SERVICES="$(for d in services/*/pkg; do
    [ -d "$d" ] || continue
    svc=$(basename "$(dirname "$d")")
    [ "$svc" = svbony-camera ] && continue
    echo "$svc"
done | tr '\n' ' ')"

# Documented family ports (mirrors verify-packages.sh port_of).
win_port_of() {
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

# Feature/component-group ids are the PascalCase service dir name.
win_feature_id() {
    echo "$1" | awk -F- '{ for (i = 1; i <= NF; i++) printf "%s%s", toupper(substr($i, 1, 1)), substr($i, 2) }'
}

pkg_wxs=installer/Package.wxs
if [ -f "$pkg_wxs" ]; then
    for svc in $WIN_SERVICES; do
        frag="installer/fragments/$svc.wxs"
        feat=$(win_feature_id "$svc")
        port=$(win_port_of "$svc")
        [ -n "$port" ] || { err "$svc: no Windows port mapping — extend win_port_of()"; continue; }
        if [ ! -f "$frag" ]; then
            err "$svc: missing $frag"
            continue
        fi
        # The element's own Name is the first Name= attribute line after the
        # opener (the fragments put one attribute per line; Id sits on the
        # opener). A bare Name= grep would also match fw:FirewallException.
        for elem in ServiceInstall ServiceControl; do
            got=$(awk -v elem="<$elem " '
                index($0, elem) { in_elem = 1; next }
                in_elem && /Name="/ { sub(/^ */, ""); print; exit }
            ' "$frag")
            [ "$got" = "Name=\"rusty-photon-$svc\"" ] \
                || err "$svc: $elem name must be rusty-photon-$svc (got: ${got:-none})"
        done
        grep -Fq "Name=\"rusty-photon-$svc.exe\" Source=\"!(bindpath.bin)\\$svc.exe\"" "$frag" \
            || err "$svc: fragment must install bindpath.bin\\$svc.exe as rusty-photon-$svc.exe"
        grep -q 'Arguments="--service"' "$frag" \
            || err "$svc: ServiceInstall must pass --service (SCM mode)"
        grep -q 'RestartServiceDelayInSeconds="5"' "$frag" \
            || err "$svc: util:ServiceConfig must restart after 5 s (systemd RestartSec=5 parity)"
        grep -q 'FailureActionsWhen="failedToStopOrReturnedError"' "$frag" \
            || err "$svc: native ServiceConfig must set the failure-actions flag (ServiceSpecific(1) exits must count as failures)"
        grep -q "Port=\"$port\"" "$frag" \
            || err "$svc: firewall exception port must be $port"
        # Demand-start on exactly the no-defaultable-config services (the
        # ConditionPathExists= translation); everything else auto-starts on
        # install. session-runner is one of the five gated services:
        # workflows_dir/state_dir are required config fields with no usable
        # defaults, mirroring its Linux ConditionPathExists= unit.
        case "$svc" in
            sky-survey-camera | plate-solver | calibrator-flats | session-runner | polar-align)
                grep -q 'Start="demand"' "$frag" \
                    || err "$svc: gated service must install with Start=\"demand\""
                grep -q 'Start="install"' "$frag" \
                    && err "$svc: gated service must not be started by the installer"
                ;;
            *)
                grep -q 'Start="auto"' "$frag" \
                    || err "$svc: service must install with Start=\"auto\""
                grep -q 'Start="install"' "$frag" \
                    || err "$svc: ServiceControl must start the service on install"
                ;;
        esac
        grep -q "ComponentGroupRef Id=\"${feat}Components\"" "$pkg_wxs" \
            || err "$svc: Package.wxs does not reference ${feat}Components (missing feature wiring)"
    done

    # Every fragment belongs to a packaged service (or the shared-payload
    # allowlist) — an orphan fragment would ship an unowned service.
    for frag in installer/fragments/*.wxs; do
        svc=$(basename "$frag" .wxs)
        case " $WIN_SERVICES " in
            *" $svc "*) ;;
            *)
                case "$svc" in
                    zwo-sdk-license) ;; # shared by the two zwo features
                    *) err "installer: orphan fragment $frag (no matching packaged service)" ;;
                esac
                ;;
        esac
    done

    # Doctor rides in Core with sentinel (no rusty-photon-doctor package):
    # the sentinel fragment must ship the exe, Package.wxs must register /
    # remove the renewal scheduled task around it, and build-msi.ps1 must
    # build the doctor crate so the bind finds the exe.
    grep -Fq 'Name="rusty-photon-doctor.exe" Source="!(bindpath.bin)\doctor.exe"' installer/fragments/sentinel.wxs \
        || err "installer: sentinel fragment must install bindpath.bin\\doctor.exe as rusty-photon-doctor.exe"
    grep -q 'Id="RegisterRenewTask"' "$pkg_wxs" \
        || err "installer: Package.wxs must register the rusty-photon-renew scheduled task"
    grep -q 'Id="UnregisterRenewTask"' "$pkg_wxs" \
        || err "installer: Package.wxs must unregister the renewal task on uninstall"
    grep -Fq 'tls renew' "$pkg_wxs" \
        || err "installer: the renewal task must run rusty-photon-doctor tls renew"
    awk '/\$allServices = @\(/, /^\)/' scripts/build-msi.ps1 | grep -Fq '"doctor"' \
        || err "installer: build-msi.ps1 \$allServices must include doctor"
fi

# ---- lifecycle-verify port tables -------------------------------------------
# verify-packages.sh (Linux) and verify-brew.sh (macOS) each keep a hand-typed
# port_of(); a service missing from one only surfaces when that platform's
# nightly verify leg actually starts the service — long after the PR that
# added it merged. Enforce membership and cross-table agreement here (the
# verifiers also fail fast on their own tables at startup). svbony-camera is
# exempt from the brew table: macOS skips it entirely (no tarball, no
# formula).
port_in() { # $1=script $2=svc — the port_of() entry, empty when absent
    sed -n "s/^ *$2) echo \([0-9][0-9]*\) ;;\$/\1/p" "$1" | head -1
}
vpk=scripts/verify-packages.sh
vbr=scripts/verify-brew.sh
if [ -f "$vpk" ] && [ -f "$vbr" ]; then
    for pkgdir in services/*/pkg; do
        [ -d "$pkgdir" ] || continue
        svc=$(basename "$(dirname "$pkgdir")")
        lport=$(port_in "$vpk" "$svc")
        if [ -z "$lport" ]; then
            err "$svc: no port mapping in $vpk port_of() — its verify leg would fail; extend it"
            continue
        fi
        [ "$svc" = svbony-camera ] && continue
        bport=$(port_in "$vbr" "$svc")
        if [ -z "$bport" ]; then
            err "$svc: no port mapping in $vbr port_of() — the macOS verify leg would fail; extend it"
        elif [ "$bport" != "$lport" ]; then
            err "$svc: port mismatch — $vpk port_of() says $lport, $vbr port_of() says $bport"
        fi
    done
fi

# The QHY Windows pin lives in four shipped places; they must all match the
# Linux pin (same SDK release linked and reported everywhere): build-msi.ps1,
# libqhyccd-sys build.rs (sdk_win64_<ver> search path), and qhy-camera's
# PINNED_SDK_VERSION (what the doctor reports as the build-time ABI).
bm=scripts/build-msi.ps1
if [ -f "$bp" ] && [ -f "$bm" ]; then
    v1=$(sed -n 's/^QHY_SDK_VERSION="\(.*\)"$/\1/p' "$bp" | head -1)
    vw=$(sed -n 's/^\$QhySdkVersion = "\(.*\)"$/\1/p' "$bm" | head -1)
    if [ -z "$vw" ] || [ "$v1" != "$vw" ]; then
        err "QHY SDK version pin mismatch: build-packages.sh='$v1' vs build-msi.ps1='$vw'"
    fi
    sysrs=crates/qhyccd-rs/libqhyccd-sys/build.rs
    grep -q "sdk_win64_$v1" "$sysrs" \
        || err "QHY SDK version pin mismatch: libqhyccd-sys build.rs has no sdk_win64_$v1 search path"
    pf=services/qhy-camera/src/preflight.rs
    vp=$(awk '/PINNED_SDK_VERSION: PinnedSdkVersion = / , /};/ {
            if ($1 == "year:") y = $2 + 0
            if ($1 == "month:") m = $2 + 0
            if ($1 == "day:") d = $2 + 0
        } END { if (y) printf "%02d.%02d.%02d", y, m, d }' "$pf")
    if [ -z "$vp" ] || [ "$v1" != "$vp" ]; then
        err "QHY SDK version pin mismatch: build-packages.sh='$v1' vs preflight PINNED_SDK_VERSION='$vp'"
    fi
fi

# The ZWO Windows SDK URLs are pinned in two shipped places (rolling CDN
# "latest" — no commit ref exists for Windows, so URL identity is the whole
# reproducibility statement): the CI action defaults and build-msi.ps1.
if [ -f "$bm" ] && [ -f "$act" ]; then
    for pair in "windows_camera_sdk_url ZwoCameraSdkUrl" "windows_eaf_sdk_url ZwoEafSdkUrl"; do
        input=${pair% *}
        var=${pair#* }
        u1=$(awk -v want="$input:" '$1 == want { in_input = 1 } in_input && $1 == "default:" { print $2; exit }' "$act")
        u2=$(sed -n "s/^\\\$$var = \"\(.*\)\"\$/\1/p" "$bm" | head -1)
        if [ -z "$u1" ] || [ "$u1" != "$u2" ]; then
            err "ZWO Windows SDK URL mismatch ($input): install-zwo-sdk='$u1' vs build-msi.ps1='$u2'"
        fi
    done
fi

if [ "$fail" -eq 0 ]; then
    echo "check-pkg-assets: OK"
fi
exit "$fail"

#!/bin/bash
# Exercise sweep_orphan_volumes. This function DELETES storage, so the cases
# that matter most are the ones where it must refuse.
#
# Fixtures here are SYNTHETIC BY POLICY. This repository is public, and these
# tests are the obvious place for a real `pvesm list` dump or a live runner id
# to get pasted in while chasing a failure on the hypervisor. Do not. Volume
# inventories and runner registrations are host state, and invented values
# exercise the code exactly as well. Every VMID, pool and slot name below
# already appears in the SLOTS array of the script under test, so nothing here
# discloses anything the script does not.
#
# The harness never needs a Proxmox host: every command that would reach one
# (`qm`, `pvesm`, ...) is stubbed, and the config filesystem is a tmpdir
# reached through PVE_CONF_ROOT. It must never grow a fallback that shells out
# to a real `qm` or `pvesm` -- that would make it depend on an environment CI
# does not have.
#
# Ordinary utilities are a different case: `find` and `awk` run for real by
# default, because a standing stub would mean testing the stub rather than the
# code. Individual cases below do shadow one on purpose to reach a failure
# path -- scoped to the case, delegating to the real binary for everything
# they are not deliberately breaking, and `unset -f` immediately after so no
# later case inherits a broken utility. So this is hermetic with respect to
# infrastructure, which is the property that matters, rather than free of the
# host entirely.
#
# Functions are lifted out of the script with `awk` rather than sourced,
# because sourcing would run the top-level slot loops.
#
# shellcheck disable=SC2034,SC2317,SC2329
# All three are structural to this harness, not per-site judgement calls. The
# constants (FREE_ATTEMPTS, ...) and the command stubs (pvesm, qm, timeout,
# awk, ...) are read and called only by the functions eval'd in from the
# script under test, which shellcheck cannot see -- so every one of them looks
# unused, uninvoked or unreachable to it. Disabling per line would mean ~30
# directives that say the same thing.
#
# SC2317 and SC2329 are the same complaint from different shellcheck releases
# (0.9 reports the stub bodies as unreachable, 0.11 as never invoked), so both
# are listed: the CI gate pins a version, but nobody's local install is
# obliged to match it.
set -u -o pipefail

SRC=${1:?path to rp-runner-pool.sh}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

FREE_ATTEMPTS=3
FREE_RETRY_SLEEP=0

eval "$(awk '/^free_leaked_volume\(\) \{/,/^\}/' "$SRC")"
eval "$(awk '/^vm_config_state\(\) \{/,/^\}/' "$SRC")"
eval "$(awk '/^sweep_orphan_volumes\(\) \{/,/^\}/' "$SRC")"

LOG=""
log() { LOG="$LOG
[$1] ${*:2}"; }
timeout() { shift 3; "$@"; }

FREED=""          # what pvesm free was asked to remove
LISTING=""        # what pvesm list returns

pvesm() {
    case $1 in
        free) FREED="$FREED $2"; return 0 ;;
        list)
            echo "Volid Format Type Size VMID"
            # After a free, stop listing what was freed.
            printf '%s\n' "$LISTING" | while IFS= read -r v; do
                [ -z "$v" ] && continue
                case " $FREED " in *" $v "*) continue ;; esac
                echo "$v raw images 4194304 9100"
            done
            return 0 ;;
    esac
}

FAILED=0

# Permission-based cases are meaningless as root: root bypasses the mode bits,
# so a directory chmod'ed 000 still reads and a 555 directory still accepts an
# unlink. Skip them loudly rather than report a failure that says nothing about
# the code under test.
AS_ROOT=0
[ "$(id -u)" -eq 0 ] && AS_ROOT=1
skipped_as_root() { echo "SKIP  $1 (running as root: mode bits do not apply)"; }
setup() { # setup <conf-root> <storage.cfg?> <vmid.conf?>
    local root=$1
    rm -rf "$root"; mkdir -p "$root/qemu-server"
    [ "$2" = yes ] && printf 'zfspool: cipool\n\tpool cipool\n' >"$root/storage.cfg"
    [ "$3" = yes ] && printf 'name: live\n' >"$root/qemu-server/9100.conf"
    return 0
}

check() { # check <desc> <expect-freed> <expect-log-substring>
    local desc=$1 expect=$2 want=$3
    if [ "$FREED" = "$expect" ] && printf '%s' "$LOG" | grep -qF "$want"; then
        echo "PASS  $desc"
    else
        echo "FAIL  $desc"
        echo "      freed=[$FREED] wanted=[$expect]"
        echo "      log:$LOG"
        FAILED=1
    fi
}

# 1. THE safety case: pmxcfs down. storage.cfg unreadable means the config
#    directory is empty too, which must never be read as "no VM exists".
setup "$TMP/r1" no no
LOG=""; FREED=""; LISTING="cipool:vm-9100-disk-0"
PVE_CONF_ROOT="$TMP/r1" sweep_orphan_volumes runner-linux1 9100
check "pmxcfs down -> frees nothing" "" "no storages readable"

# 2. The VM still has a config: the reconcile owns it, not this.
setup "$TMP/r2" yes yes
LOG=""; FREED=""; LISTING="cipool:vm-9100-disk-0"
PVE_CONF_ROOT="$TMP/r2" sweep_orphan_volumes runner-linux1 9100
check "VM config still present -> frees nothing" "" "still has a VM config"

# 3. A volume listed against the VMID that is not one of its own is left alone.
setup "$TMP/r3" yes no
LOG=""; FREED=""; LISTING="cipool:base-926-disk-0"
PVE_CONF_ROOT="$TMP/r3" sweep_orphan_volumes runner-linux1 9100
check "base image never touched" "" "leaving unexpected volume"

# 4. The real case: no config, storages readable, own volumes -> freed.
setup "$TMP/r4" yes no
LOG=""; FREED=""; LISTING="cipool:vm-9100-disk-0
cipool:vm-9100-cloudinit"
PVE_CONF_ROOT="$TMP/r4" sweep_orphan_volumes runner-linux1 9100
check "orphans freed and confirmed" " cipool:vm-9100-disk-0 cipool:vm-9100-cloudinit" "confirmed gone from the storage"

# 5. Linked-clone volid form (a '/' in the name) is matched too.
setup "$TMP/r5" yes no
LOG=""; FREED=""; LISTING="cipool:base-926-disk-0/vm-9100-disk-0"
PVE_CONF_ROOT="$TMP/r5" sweep_orphan_volumes runner-linux1 9100
check "linked-clone volid freed" " cipool:base-926-disk-0/vm-9100-disk-0" "confirmed gone from the storage"

# 6. storage.cfg yields some section names and THEN fails. Non-empty output is
#    not a healthy parse: a truncated storage list means the config filesystem
#    is not reliably readable, which is the condition that makes an empty
#    qemu-server directory unsafe to read as "the VM is gone".
setup "$TMP/r6" yes no
REAL_AWK=$(command -v awk)
awk() { # only the storage.cfg parse fails; the volume matcher must still work
    case "${*: -1}" in
        */storage.cfg) echo cipool; return 2 ;;
    esac
    "$REAL_AWK" "$@"
}
LOG=""; FREED=""; LISTING="cipool:vm-9100-disk-0"
PVE_CONF_ROOT="$TMP/r6" sweep_orphan_volumes runner-linux1 9100
check "storage.cfg parse fails after emitting names -> frees nothing" "" "no storages readable"
unset -f awk

# 7. A storage that will not list is the likeliest reason this slot keeps
#    failing to clone. Skipping it silently leaves the journal with nothing
#    but the clone failure.
setup "$TMP/r7" yes no
REAL_PVESM=$(declare -f pvesm)
pvesm() { case $1 in free) FREED="$FREED $2"; return 0 ;; list) return 1 ;; esac; }
LOG=""; FREED=""; LISTING=""
PVE_CONF_ROOT="$TMP/r7" sweep_orphan_volumes runner-linux1 9100
check "storage will not list -> logged, not skipped silently" "" "could not list storage 'cipool'"
eval "$REAL_PVESM"

# 8. THE time-of-check/time-of-use case: a VM config appears between the check
#    that licensed the sweep and the free. Those volumes may be live now.
setup "$TMP/r8" yes no
REAL_PVESM=$(declare -f pvesm)
pvesm() {
    case $1 in
        free) FREED="$FREED $2"; return 0 ;;
        list)
            printf 'name: live\n' >"$TMP/r8/qemu-server/9100.conf"   # recreated mid-sweep
            echo "Volid Format Type Size VMID"
            echo "cipool:vm-9100-disk-0 raw images 4194304 9100"
            return 0 ;;
    esac
}
LOG=""; FREED=""; LISTING=""
PVE_CONF_ROOT="$TMP/r8" sweep_orphan_volumes runner-linux1 9100
check "config appearing mid-sweep stops it before the free" "" "a VM config appeared while it was running"
eval "$REAL_PVESM"

# 9. The config directory cannot be listed. `[ -e ]` on the config file alone
#    reads that as "no VM", which is the licence to delete a live clone's
#    disks -- so a directory that will not answer must refuse, not proceed.
if [ "$AS_ROOT" = 1 ]; then
    skipped_as_root "unreadable config directory -> frees nothing"
else
    setup "$TMP/r9" yes no
    chmod 000 "$TMP/r9/qemu-server"
    LOG=""; FREED=""; LISTING="cipool:vm-9100-disk-0"
    PVE_CONF_ROOT="$TMP/r9" sweep_orphan_volumes runner-linux1 9100
    chmod 755 "$TMP/r9/qemu-server"
    check "unreadable config directory -> frees nothing" "" "would not answer"
fi

# 10. The same, appearing mid-sweep: the recheck before each free must fail
#     closed for an unreadable directory, not only for a config that exists.
if [ "$AS_ROOT" = 1 ]; then
    skipped_as_root "config directory going unreadable mid-sweep stops it"
else
    setup "$TMP/r10" yes no
    REAL_PVESM=$(declare -f pvesm)
    pvesm() {
        case $1 in
            free) FREED="$FREED $2"; return 0 ;;
            list)
                chmod 000 "$TMP/r10/qemu-server"   # stops answering mid-sweep
                echo "Volid Format Type Size VMID"
                echo "cipool:vm-9100-disk-0 raw images 4194304 9100"
                return 0 ;;
        esac
    }
    LOG=""; FREED=""; LISTING=""
    PVE_CONF_ROOT="$TMP/r10" sweep_orphan_volumes runner-linux1 9100
    chmod 755 "$TMP/r10/qemu-server"
    check "config directory going unreadable mid-sweep stops it" "" "stopped answering"
    eval "$REAL_PVESM"
fi

exit "$FAILED"

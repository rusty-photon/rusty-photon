#!/bin/bash
# Exercise free_leaked_volume against stubbed pvesm behaviour.
#   0 = confirmed gone, 1 = confirmed still present, 2 = could not confirm,
#   3 = matcher failed, 4 = stopped, the VMID owns a config again.
#
# Fixtures here are SYNTHETIC BY POLICY. This repository is public, and these
# tests are the obvious place for a real `pvesm list` dump or a live runner id
# to get pasted in while chasing a failure on the hypervisor. Do not. Volume
# inventories and runner registrations are host state, and invented values
# exercise the code exactly as well. Every VMID, pool and slot name below
# already appears in the SLOTS array of the script under test, so nothing here
# discloses anything the script does not.
#
# The harness also stays hermetic: every external command it depends on is
# stubbed, and the config filesystem is a tmpdir reached through PVE_CONF_ROOT.
# It never needs a Proxmox host, and must never grow a fallback that shells out
# to a real `qm` or `pvesm` -- that would make it depend on an environment CI
# does not have.
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

# A live config filesystem with no VM config for 9100: the invariant every
# caller holds on entry. storage.cfg readable is the pmxcfs liveness signal --
# without it an empty qemu-server proves nothing.
PVE_CONF_ROOT="$TMP/pve"
mkdir -p "$PVE_CONF_ROOT/qemu-server"
printf 'zfspool: cipool\n\tpool cipool\n' >"$PVE_CONF_ROOT/storage.cfg"

eval "$(awk '/^vm_config_state\(\) \{/,/^\}/' "$SRC")"
eval "$(awk '/^free_leaked_volume\(\) \{/,/^\}/' "$SRC")"
eval "$(awk '/^destroy_clone_holding_id\(\) \{/,/^\}/' "$SRC")"

timeout() { shift 3; "$@"; }
log() { :; }
swept=""
sweep_orphan_volumes() { swept="$1/$2"; }

VOL=cipool:vm-9100-cloudinit
STILL_PRESENT_UNTIL=0
LIST_BROKEN=0
TRAILING_ROWS=0        # rows emitted AFTER the match -> SIGPIPE bait
attempts=0

pvesm() {
    case $1 in
        free) attempts=$((attempts + 1)); return 0 ;;   # always "succeeds"
        list)
            [ "$LIST_BROKEN" = 1 ] && return 1
            echo "Volid Format Type Size VMID"
            if [ "$STILL_PRESENT_UNTIL" -lt 0 ] ||
               [ "$attempts" -le "$STILL_PRESENT_UNTIL" ]; then
                echo "$VOL raw images 4194304 9100"
                if [ "$TRAILING_ROWS" -gt 0 ]; then
                    seq 1 "$TRAILING_ROWS" |
                      sed 's#^#cipool:filler-#; s#$# raw images 1 9100#'
                fi
            fi
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
run() {
    local desc=$1 expect=$2 rc
    attempts=0
    free_leaked_volume 9100 "$VOL"; rc=$?
    if [ "$rc" = "$expect" ]; then
        echo "PASS  $desc (rc=$rc, free called ${attempts}x)"
    else
        echo "FAIL  $desc (rc=$rc, wanted $expect, free called ${attempts}x)"
        FAILED=1
    fi
}

STILL_PRESENT_UNTIL=0; LIST_BROKEN=0; TRAILING_ROWS=0
run "volume genuinely freed on attempt 1 -> confirmed gone" 0

STILL_PRESENT_UNTIL=-1; LIST_BROKEN=0; TRAILING_ROWS=0
run "free lies (exit 0, volume persists) -> confirmed still present" 1

STILL_PRESENT_UNTIL=1; LIST_BROKEN=0; TRAILING_ROWS=0
run "volume clears on attempt 2 -> retry earns the success" 0

STILL_PRESENT_UNTIL=0; LIST_BROKEN=1; TRAILING_ROWS=0
run "storage will not list -> could not confirm (not 'still listed')" 2

# The regression Copilot found: a match followed by more output used to make
# the pipeline return 141 under pipefail, which read as "gone".
STILL_PRESENT_UNTIL=-1; LIST_BROKEN=0; TRAILING_ROWS=200000
run "volume present with 200k rows after it -> still present, no SIGPIPE lie" 1

# A matcher that fails to run is not evidence of removal. Stub awk non-zero
# with a status that is NOT 1 (1 means a clean no-match). Only the MATCHER is
# broken -- vm_config_state parses storage.cfg with awk too, and breaking that
# would stop the run before it ever reached the matcher.
REAL_AWK=$(command -v awk)
awk() {
    case "$*" in
        *-v\ want=*) return 2 ;;
    esac
    "$REAL_AWK" "$@"
}
STILL_PRESENT_UNTIL=-1; LIST_BROKEN=0; TRAILING_ROWS=0
run "matcher errors on a listable storage -> verdict 3, not 2" 3
unset -f awk

# And a broken storage still reports 2, so the two causes stay distinguishable.
STILL_PRESENT_UNTIL=-1; LIST_BROKEN=1; TRAILING_ROWS=0
run "storage unreadable -> verdict 2, distinct from matcher failure" 2

# The retry window is minutes of REPEATED deletion, so ownership is rechecked
# before every attempt, not once by the caller. The sequence that bites: the
# free worked, the listing was merely flaky, and the freed name went to a VM
# recreated meanwhile -- the next attempt would delete that VM's volume.
STILL_PRESENT_UNTIL=-1; LIST_BROKEN=0; TRAILING_ROWS=0
printf 'name: live\n' >"$PVE_CONF_ROOT/qemu-server/9100.conf"
run "VM config reappears mid-retry -> stopped (verdict 4)" 4
before=$attempts
rm -f "$PVE_CONF_ROOT/qemu-server/9100.conf"

# ...and it stops on the FIRST attempt, rather than freeing once and then
# noticing. A recheck that runs after the deletion buys nothing.
if [ "$before" = 0 ]; then
    echo "PASS  a reappeared config stops the free before it runs, not after"
else
    echo "FAIL  a reappeared config stops the free before it runs, not after ($before frees)"
    FAILED=1
fi

# A config view that stops answering is a DIFFERENT stop from a VM appearing,
# and gets its own verdict: one sends an operator to whoever made the VM, the
# other to pmxcfs. Collapsing them would name the wrong cause.
if [ "$AS_ROOT" = 1 ]; then
    skipped_as_root "config directory unreadable mid-retry -> stopped (verdict 5)"
else
    STILL_PRESENT_UNTIL=-1; LIST_BROKEN=0; TRAILING_ROWS=0
    chmod 000 "$PVE_CONF_ROOT/qemu-server"
    run "config directory unreadable mid-retry -> stopped (verdict 5)" 5
    chmod 755 "$PVE_CONF_ROOT/qemu-server"
fi

# THE case the per-attempt vm_config_state check exists for: pmxcfs going down
# mid-retry leaves a qemu-server that is empty AND enumerable, so the config
# test alone would read "no VM" and license the next deletion.
STILL_PRESENT_UNTIL=-1; LIST_BROKEN=0; TRAILING_ROWS=0
mv "$PVE_CONF_ROOT/storage.cfg" "$TMP/storage.cfg.away"
run "config filesystem goes down mid-retry -> stopped (verdict 5)" 5
mv "$TMP/storage.cfg.away" "$PVE_CONF_ROOT/storage.cfg"

# And a storage.cfg that emits a name then fails is not a live filesystem
# either -- same reasoning as the sweep's own parse check.
STILL_PRESENT_UNTIL=-1; LIST_BROKEN=0; TRAILING_ROWS=0
REAL_AWK=$(command -v awk)
awk() {
    case "${*: -1}" in
        */storage.cfg) echo cipool; return 2 ;;
    esac
    "$REAL_AWK" "$@"
}
run "storage.cfg parse fails mid-retry -> stopped (verdict 5)" 5
unset -f awk

# THE within-function race: the directory probe and the config test used to be
# separate reads, so pmxcfs could go away between them -- and the second read
# failing looks exactly like the config file being absent, which is the answer
# that licenses a delete. Simulate by breaking `find` alone.
STILL_PRESENT_UNTIL=-1; LIST_BROKEN=0; TRAILING_ROWS=0
find() { return 1; }
run "config read fails mid-probe -> stopped, not read as absent" 5
unset -f find

# A config directory that is a regular FILE must not pass as enumerable:
# `ls -A` on a file succeeds by listing it, and the config test is then false
# for every VMID -- an enumeration that "worked" licensing every deletion.
STILL_PRESENT_UNTIL=-1; LIST_BROKEN=0; TRAILING_ROWS=0
rmdir "$PVE_CONF_ROOT/qemu-server"
printf 'not a directory\n' >"$PVE_CONF_ROOT/qemu-server"
run "config directory is a regular file -> stopped, not licensed" 5
rm -f "$PVE_CONF_ROOT/qemu-server"
mkdir -p "$PVE_CONF_ROOT/qemu-server"

# destroy_clone_holding_id must keep passing the SAME id across deferrals,
# because a later reconcile has no marker left to recover it from. The VM has
# to still EXIST for this to be the case under test -- with no config the
# helper now correctly stops instead of retrying (see below).
printf 'name: live\n' >"$PVE_CONF_ROOT/qemu-server/9100.conf"
calls=0
ids=""
destroy_clone() {            # fails twice, then succeeds
    calls=$((calls + 1))
    ids="$ids $2"
    [ "$calls" -ge 3 ]
}
sleep() { :; }               # no real backoff in the test
destroy_clone_holding_id 9100 "rid-42"
if [ "$calls" = 3 ] && [ "$ids" = " rid-42 rid-42 rid-42" ]; then
    echo "PASS  deferred teardown retries holding the same id ($calls calls:$ids)"
else
    echo "FAIL  deferred teardown (calls=$calls ids=[$ids])"
    FAILED=1
fi
unset -f destroy_clone sleep
rm -f "$PVE_CONF_ROOT/qemu-server/9100.conf"

# THE unbounded-retry case. destroy_clone defers when the storage gate cannot
# read the VM's config -- including when that config is GONE, which a
# `qm destroy` that removed it and still exited non-zero reaches with nobody's
# help. Retrying forever there loses the slot permanently.
calls=0
swept=""
destroy_clone() { calls=$((calls + 1)); return 1; } # never succeeds
sleep() { :; }
destroy_clone_holding_id 9100 "rid-42"
if [ "$calls" = 1 ] && [ "$swept" = "9100/9100" ]; then
    echo "PASS  teardown of an already-gone VM stops retrying, and sweeps it"
else
    echo "FAIL  teardown of an already-gone VM stops retrying, and sweeps it"
    echo "      destroy_clone calls=$calls (wanted 1), swept=[$swept]"
    FAILED=1
fi

# ...and it must NOT stop while the config view merely cannot be read: that is
# the transient case the retry exists for, and treating it as "already gone"
# would abandon a VM that still exists.
calls=0
swept=""
mv "$PVE_CONF_ROOT/storage.cfg" "$TMP/storage.cfg.away2"
tries=0
destroy_clone() {
    calls=$((calls + 1))
    tries=$((tries + 1))
    [ "$tries" -ge 3 ] # defers twice, then takes
}
destroy_clone_holding_id 9100 "rid-42"
mv "$TMP/storage.cfg.away2" "$PVE_CONF_ROOT/storage.cfg"
if [ "$calls" = 3 ] && [ -z "$swept" ]; then
    echo "PASS  an unreadable config view keeps retrying, it is not 'gone'"
else
    echo "FAIL  an unreadable config view keeps retrying, it is not 'gone'"
    echo "      destroy_clone calls=$calls (wanted 3), swept=[$swept]"
    FAILED=1
fi

exit "$FAILED"

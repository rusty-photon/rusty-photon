#!/bin/bash
# Exercise destroy_clone's runner-id recovery and its exit status. Both decide
# something irreversible: which runner gets deregistered, and whether the
# caller believes the VM is gone.
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
# shellcheck disable=SC2034,SC2329
# Both are structural to this harness, not per-site judgement calls. The
# constants (FREE_ATTEMPTS, ...) and the command stubs (pvesm, qm, timeout,
# awk, ...) are read and called only by the functions eval'd in from the
# script under test, which shellcheck cannot see -- so every one of them
# looks unused or uninvoked to it. Disabling per line would mean ~30
# directives that say the same thing.
set -u -o pipefail

SRC=${1:?path to rp-runner-pool.sh}
TMP=$(mktemp -d)
trap 'chmod -R u+w "$TMP" 2>/dev/null; rm -rf "$TMP"' EXIT

STATE_DIR="$TMP/state"; mkdir -p "$STATE_DIR"
FW_DIR="$TMP/fw"; mkdir -p "$FW_DIR"
FREE_ATTEMPTS=1
FREE_RETRY_SLEEP=0

# free_leaked_volume rechecks ownership, so its dependencies come too.
PVE_CONF_ROOT="$TMP/pve"
mkdir -p "$PVE_CONF_ROOT/qemu-server"

eval "$(awk '/^vm_config_state\(\) \{/,/^\}/' "$SRC")"
eval "$(awk '/^free_leaked_volume\(\) \{/,/^\}/' "$SRC")"
eval "$(awk '/^read_marker\(\) \{/,/^\}/' "$SRC")"
eval "$(awk '/^destroy_clone\(\) \{/,/^\}/' "$SRC")"

LOG=""
DEREGISTERED=""
# deregister_runner is called inside $( ), so a shell variable cannot carry the
# record back out of that subshell. A file can.
DEREG_FILE="$TMP/dereg"
: >"$DEREG_FILE"
log() { LOG="$LOG
[$1] ${*:2}"; }
qm() { case $1 in destroy) echo "ok" ;; esac; return 0; }
timeout() { shift 3; "$@"; }
pvesm() { case $1 in list) echo "Volid Format Type Size VMID" ;; esac; return 0; }
storage_gate() { echo "cipool"; return 0; }
deregister_runner() { printf ' %s' "$1" >>"$DEREG_FILE"; echo 204; }

FAILED=0

# Permission-based cases are meaningless as root: root bypasses the mode bits,
# so a directory chmod'ed 000 still reads and a 555 directory still accepts an
# unlink. Skip them loudly rather than report a failure that says nothing about
# the code under test.
AS_ROOT=0
[ "$(id -u)" -eq 0 ] && AS_ROOT=1
skipped_as_root() { echo "SKIP  $1 (running as root: mode bits do not apply)"; }
check() { # check <desc> <expect-dereg> <expect-rc> <actual-rc>
  local desc=$1 expect=$2 wantrc=$3 gotrc=$4
  DEREGISTERED=$(cat "$DEREG_FILE")
  if [ "$DEREGISTERED" = "$expect" ] && [ "$gotrc" = "$wantrc" ]; then
    echo "PASS  $desc"
  else
    echo "FAIL  $desc"
    echo "      dereg=[$DEREGISTERED] wanted=[$expect]  rc=$gotrc wanted=$wantrc"
    echo "      log:$LOG"
    FAILED=1
  fi
}
reset() { LOG=""; : >"$DEREG_FILE"; rm -f "$STATE_DIR"/* "$FW_DIR"/*; }

# 1. The published marker is what normally names the runner.
reset
printf '4242\n' >"$STATE_DIR/9100.injected"
printf '9999\n' >"$STATE_DIR/9100.injected.tmp"
destroy_clone 9100; rc=$?
check "published marker wins over an unpublished one" " 4242" 0 "$rc"

# 2. THE new case: killed between the write and the rename. The id exists only
#    in .tmp, and without reading it the runner outlives its clone.
reset
printf '4242\n' >"$STATE_DIR/9100.injected.tmp"
destroy_clone 9100; rc=$?
check "unpublished marker recovers the id" " 4242" 0 "$rc"

# 3. THE safety case for that: a .tmp whose write never finished holds a
#    prefix, and a prefix of an id is a different runner's id.
reset
printf '4242' >"$STATE_DIR/9100.injected.tmp" # no trailing newline
destroy_clone 9100; rc=$?
check "truncated unpublished marker deregisters nothing" "" 0 "$rc"

# 4. Nothing to read: unchanged behaviour.
reset
destroy_clone 9100; rc=$?
check "no marker at all -> no deregistration" "" 0 "$rc"

# 5. An explicit id beats both files (the injection-failure path).
reset
printf '4242\n' >"$STATE_DIR/9100.injected"
destroy_clone 9100 777; rc=$?
check "explicit id wins" " 777" 0 "$rc"

# 6. Both marker files are consumed, so neither can be read as a later clone's.
reset
printf '4242\n' >"$STATE_DIR/9100.injected"
printf '4242\n' >"$STATE_DIR/9100.injected.tmp"
destroy_clone 9100 >/dev/null
if [ -e "$STATE_DIR/9100.injected" ] || [ -e "$STATE_DIR/9100.injected.tmp" ]; then
  echo "FAIL  both marker files consumed"
  FAILED=1
else
  echo "PASS  both marker files consumed"
fi

# 7. A PUBLISHED marker can be short too -- one written in place by an older
#    build of this script, surviving in /run across the upgrade. A prefix of a
#    runner id names a different runner, so it must not be used either.
reset
printf '4242' >"$STATE_DIR/9100.injected" # no trailing newline
destroy_clone 9100; rc=$?
check "truncated published marker deregisters nothing" "" 0 "$rc"

# 8. A marker that cannot be removed is named, because the reconcile will read
#    it as proof this clone holds a live job.
if [ "$AS_ROOT" = 1 ]; then
  skipped_as_root "unremovable marker is logged, not left silent"
else
  reset
  printf '4242\n' >"$STATE_DIR/9100.injected"
  chmod 555 "$STATE_DIR"
  destroy_clone 9100 >/dev/null
  chmod 755 "$STATE_DIR"
  if printf '%s' "$LOG" | grep -qF "could not clear the injection marker"; then
    echo "PASS  unremovable marker is logged, not left silent"
  else
    echo "FAIL  unremovable marker is logged, not left silent"
    echo "      log:$LOG"
    FAILED=1
  fi
fi

# 9. THE status case: firewall cleanup failing after a successful destroy must
#    not read as "the VM is still there" -- destroy_clone_holding_id would then
#    retry forever and the slot would be lost.
if [ "$AS_ROOT" = 1 ]; then
  skipped_as_root "unremovable firewall file still returns a completed destroy"
else
  reset
  printf 'rules' >"$FW_DIR/9100.fw"
  chmod 555 "$FW_DIR"
  destroy_clone 9100; rc=$?
  chmod 755 "$FW_DIR"
  if [ "$rc" = 0 ] && printf '%s' "$LOG" | grep -qF "could not remove the firewall policy"; then
    echo "PASS  unremovable firewall file still returns a completed destroy"
  else
    echo "FAIL  unremovable firewall file still returns a completed destroy (rc=$rc)"
    echo "      log:$LOG"
    FAILED=1
  fi
fi

exit "$FAILED"

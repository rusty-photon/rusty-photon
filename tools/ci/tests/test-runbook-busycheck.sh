#!/bin/bash
# The busy-check in the runbook decides whether an operator runs `qm stop` on a
# clone that may be working a job. Run the snippet exactly as documented.
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
set -u

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
M="$TMP/9100.injected"

probe() { # the documented snippet, verbatim apart from the marker path
  local rid=""
  IFS= read -r rid 2>/dev/null <"$M" || rid=""
  case "$rid" in '' | *[!0-9]*) rid="" ;; esac
  if [ -z "$rid" ]; then
    echo "refuses"
  else
    echo "queries $rid"
  fi
}

FAILED=0
check() { # check <desc> <want>
  local got
  got=$(probe)
  if [ "$got" = "$2" ]; then
    echo "PASS  $1"
  else
    echo "FAIL  $1 (got '$got', wanted '$2')"
    FAILED=1
  fi
}

printf '4242\n' >"$M"
check "complete marker -> asks about that runner" "queries 4242"

printf '4242' >"$M" # interrupted in-place write: a PREFIX of some id
check "unterminated marker -> refuses, does not ask about a stranger" "refuses"

: >"$M"
check "empty marker -> refuses" "refuses"

rm -f "$M"
check "missing marker -> refuses" "refuses"

printf 'abc\n' >"$M"
check "non-numeric marker -> refuses" "refuses"

exit "$FAILED"
